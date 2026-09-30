//! Every chain at once, in the shape of the `payment-server` composition
//! root: one document, one registration line per chain, one composition over
//! one index file, then one sync loop, one watched address and one send per
//! chain.
//!
//! What comes back after a restart has two owners. What is *observed* comes
//! back from the index's own rows, at no cost to this file. What can *sign*
//! comes back from the application's key material — here, fixed seeds; in a
//! server, a key store. Needs all three local nodes (examples/README.md):
//! `cargo run -p payment-examples --example all_chains`

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use indexing::{
    Birthday, Page, Subject, Supervisor, SyncReport, Watching as _,
    sqlite::{Database, Repository},
};
use json_rpc::Client as _;
use sdk_config::{Document, Registry};
use serde_json::{Value, json};
use tokio::sync::watch;
use wallets::{Composition, SecretBytes, SubmissionRegistrar, Submissions, Wallet};

type AnyError = Box<dyn std::error::Error + Send + Sync>;

/// The chains this scenario runs and the node of each.
const CONFIG: &str = include_str!("config.toml");

const INTERVAL: Duration = Duration::from_secs(1);
/// Bitcoin coinbase outputs require 100 blocks before they are spendable.
const MATURITY: u64 = 100;
/// One SOL in lamports.
const AIRDROP: u64 = 1_000_000_000;
/// A well-known Ethereum address, observed without a key.
const WATCHED: &str = "0x000000000000000000000000000000000000dead";

/// Each chain's part: its asset tag, the sending wallet and its seed, the
/// receiving wallet and its seed, and the amount sent in display units.
///
/// The seeds are this scenario's stand-in for a key store, so a second run
/// rebuilds the same wallets instead of minting new ones. Ethereum is funded
/// through a transaction so the index observes its balance.
const TRANSFERS: [(&str, Held, Held, &str); 3] = [
    ("btc", ("btc-alice", 0x11), ("btc-bob", 0x12), "0.5"),
    ("eth", ("eth-alice", 0x16), ("eth-bob", 0x13), "0.01"),
    ("sol", ("sol-alice", 0x14), ("sol-bob", 0x15), "0.1"),
];

/// A wallet id and the byte its 32-byte seed repeats.
type Held = (&'static str, u8);

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    // 1. Local testing only: a `solana-test-validator --reset` is a new
    //    cluster every run, so `config.toml` cannot state its genesis hash and
    //    the node is asked for it. A deployment writes the hash it expects.
    let sections: Value = Document::toml(CONFIG).parse()?;
    let genesis = call(&sections, "solana", "getGenesisHash", json!([])).await?;
    let genesis = genesis.as_str().ok_or("a genesis hash is text")?;
    let document = Document::toml(CONFIG.replace("<genesis-hash-from-the-node>", genesis));

    // 2. The chains this application links, one line each. Each parses the
    //    section registered under its name; asset tags are this file's own.
    let mut registry = Registry::<String, Repository>::new();
    registry.register::<chain_bitcoin::Config<String>>("bitcoin");
    registry.register::<chain_ethereum::Config<String>>("ethereum");
    registry.register::<chain_solana::Config<String>>("solana");
    let chains = registry.chains(&document.parse()?)?;

    // 3. Compose. Every node's identity is verified before the store is
    //    opened, then each chain builds its index and factories over it.
    //    `submissions` owns the send work a chain finishes in the background.
    let submissions = Arc::new(Submissions::new());
    let registrar: Arc<dyn SubmissionRegistrar> = submissions.clone();
    let Composition {
        index,
        history,
        balance: balances,
        wallets,
        units,
        schedules,
        ..
    } = chains
        .compose(
            &BTreeMap::new(),
            || async {
                let file = std::path::absolute(".index-data/all_chains.db")?;
                tokio::task::spawn_blocking(move || -> Result<Database, AnyError> {
                    std::fs::create_dir_all(".index-data")?;
                    Ok(Database::open(file)?)
                })
                .await?
            },
            |store: &Database| Ok::<_, AnyError>(Repository::new(store.clone())),
            &registrar,
        )
        .await?;

    // 4. What a previous run observed is already back: the index read its own
    //    rows before its first pass, and this file did nothing for it.
    for network in index.networks() {
        let watched = index.watched(network).await?;
        println!("{}: {} address(es) restored", network.chain, watched.len());
    }

    // 5. A sender and a receiver per chain, rebuilt from this scenario's own
    //    key material. Watching an address again under the same id and
    //    birthday changes nothing, so a second run states the same thing.
    let mut signers: BTreeMap<&str, Arc<dyn Wallet>> = BTreeMap::new();
    for (asset, sender, receiver, _) in TRANSFERS {
        let asset = asset.to_owned();
        for (id, seed) in [sender, receiver] {
            let secret = SecretBytes::new(vec![seed; 32]);
            let birthday = Birthday::Fresh;
            let wallet = wallets.from(&asset, secret).await?;
            index.watch(id, wallet.as_ref(), birthday).await?;
            signers.insert(id, wallet);
        }
    }

    // 6. One watched address: history only, no key, no sends.
    let ether = "eth".to_owned();
    let watched = Subject {
        address: wallets.parse(&ether, WATCHED)?,
        asset: wallets.asset(&ether)?.id,
    };
    index
        .watch("eth-watched", &watched, Birthday::Fresh)
        .await?;

    // 7. Funding is a local-network operation and not part of the SDK:
    //    bitcoin mines past coinbase maturity, solana asks for an airdrop.
    let depth = sections["bitcoin"]["confirmation_depth"]
        .as_u64()
        .ok_or("bitcoin confirmation_depth must be explicit")?;
    let mined = json!([MATURITY + depth + 1, signers["btc-alice"].address().value]);
    call(&sections, "bitcoin", "generatetoaddress", mined).await?;
    let airdrop = json!([signers["sol-alice"].address().value, AIRDROP]);
    call(&sections, "solana", "requestAirdrop", airdrop).await?;
    let accounts = call(&sections, "ethereum", "eth_accounts", json!([])).await?;
    let funder = accounts[0]
        .as_str()
        .ok_or("Anvil needs an unlocked account")?;
    let deposit = json!([{
        "from": funder,
        "to": signers["eth-alice"].address().value,
        "value": "0xde0b6b3a7640000"
    }]);
    call(&sections, "ethereum", "eth_sendTransaction", deposit).await?;

    // 8. One sync loop over the composed index serves every address, watched
    //    or held; wait until every network has reached its node's tip.
    let (stop, stopped) = watch::channel(false);
    let (monitor, mut readiness) = SyncReport::watching(index.networks());
    let sync = tokio::spawn(Supervisor::new(index.clone(), schedules, monitor).run(stopped));
    let health = readiness.settled().await.ok_or("synchronization stopped")?;
    if let Some(error) = health.networks().iter().find_map(|row| row.failure.clone()) {
        return Err(error.into());
    }

    // 9. One send per chain, from the wallet this file holds, once the index
    //    has seen its funds. The map hands out an author only for an asset
    //    this system sends.
    for (asset, (sender, _), (receiver, _), amount) in TRANSFERS {
        let asset = asset.to_owned();
        while balances
            .balance(signers[sender].as_ref())
            .await?
            .amount
            .is_zero()
        {
            tokio::time::sleep(INTERVAL).await;
        }
        let to = wallets.parse(&asset, &signers[receiver].address().value)?;
        let author = wallets.author(&asset, &signers[sender])?;
        let id = author
            .send(to, amount.parse::<base::Decimal>()?.into())
            .await?;
        println!("{sender} sent {amount} to {receiver} in {id}");
    }

    // 10. The watched address is served like any other.
    let page = history.history(&watched, Page::first(10)).await?;
    let unit = units
        .of(&watched.address.network, &watched.asset)
        .ok_or("ether's unit")?;
    println!(
        "{WATCHED}: {} transaction(s), fees in {} digits",
        page.transactions.len(),
        unit.decimals
    );

    // Finish ambiguous submissions while indexing can still reconcile them.
    submissions.drain().await?;
    stop.send(true)?;
    sync.await?;
    Ok(())
}

/// One raw call to the node `config.toml` names for `chain`. Local-network
/// setup only — the validator's genesis hash, mining, an airdrop — and never
/// SDK API.
async fn call(
    sections: &Value,
    chain: &str,
    method: &str,
    params: Value,
) -> Result<Value, AnyError> {
    let url = sections[chain]["rpc"]["endpoints"][0]
        .as_str()
        .ok_or("the section states rpc.endpoints")?;
    let node = json_rpc::Endpoint::new(url).connect(&BTreeMap::new())?;
    let result = node
        .request(json_rpc::Request::once(method, params))
        .await?;
    Ok(result.map_err(json_rpc::CallError::from)?.deserialize()?)
}
