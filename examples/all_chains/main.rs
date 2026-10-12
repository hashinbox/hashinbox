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
    Birthday, Driver, Subject, Watching as _,
    sqlite::{Database, Repository},
};
use json_rpc::Client as _;
use payments::config::{Composition, Document, Registry};
use serde_json::{Value, json};
use tokio::sync::watch;
use wallets::WalletRepository as _;
use wallets::{SecretBytes, SubmissionRegistrar, Submissions, Wallet};

type AnyError = Box<dyn std::error::Error + Send + Sync>;

/// The chains this scenario runs and the node of each.
const CONFIG: &str = include_str!("config.toml");

const INTERVAL: Duration = Duration::from_secs(1);
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
    // The file is opened here so its wallet rows can be filed: a watch
    // observes a wallet the store already holds.
    let file = std::path::absolute(".index-data/all_chains.db")?;
    tokio::task::spawn_blocking(|| std::fs::create_dir_all(".index-data")).await??;
    let database = Database::open(file).await?;
    sqlx::raw_sql(include_str!("../../schema/sdk.sql"))
        .execute(database.pool())
        .await?;
    let filed = wallets::storage::sqlite::Repository::new(database.pool().clone());
    let indexing = Repository::new(database.clone());
    let Composition {
        index,
        wallets,
        units,
        ..
    } = chains
        .compose(
            || {
                let database = database.clone();
                async move { Ok::<_, AnyError>(database) }
            },
            |_: &Database| Ok::<_, AnyError>(indexing.clone()),
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
            let wallet = wallets.from_pk(&asset, secret).await?;
            filed
                .register(wallets::Registration {
                    id: id.to_owned(),
                    address: wallet.address().clone(),
                    label: None,
                    key: None,
                })
                .await?;
            index.watch(id, wallet.as_ref(), birthday).await?;
            signers.insert(id, wallet);
        }
    }

    // 6. One watched address: observed, no key, no sends.
    let ether = "eth".to_owned();
    let watched = Subject {
        address: wallets.parse(&ether, WATCHED)?,
        asset: wallets.asset(&ether)?.id,
    };
    filed
        .register(wallets::Registration {
            id: "eth-watched".to_owned(),
            address: watched.address.clone(),
            label: None,
            key: None,
        })
        .await?;
    index
        .watch("eth-watched", &watched, Birthday::Fresh)
        .await?;

    // 7. Funding is a local-network operation and not part of the SDK:
    //    use the node's funded Bitcoin wallet and Solana's airdrop.
    let coinbase = call(&sections, "bitcoin", "getnewaddress", json!([])).await?;
    let depth = sections["bitcoin"]["confirmation_depth"]
        .as_u64()
        .ok_or("bitcoin confirmation_depth must be explicit")?;
    let deposit = json!([signers["btc-alice"].address().value, 1]);
    call(&sections, "bitcoin", "sendtoaddress", deposit).await?;
    let mined = json!([depth + 1, coinbase]);
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
    let driver = Driver::new(index.clone());
    let mut readiness = driver.readiness();
    let sync = tokio::spawn(driver.run(stopped));
    let health = readiness.settled().await.ok_or("synchronization stopped")?;
    if let Some(error) = health.networks().iter().find_map(|row| row.failure.clone()) {
        return Err(error.into());
    }

    // 9. One send per chain, from the wallet this file holds, retried until
    //    the index has seen its funds. The map hands out an author only for
    //    an asset this system sends.
    for (asset, (sender, _), (receiver, _), amount) in TRANSFERS {
        let asset = asset.to_owned();
        let to = wallets.parse(&asset, &signers[receiver].address().value)?;
        let author = wallets.author(&asset, &signers[sender])?;
        let id = send_when_funded(author.as_ref(), &to, amount).await?;
        println!("{sender} sent {amount} to {receiver} in {id}");
    }

    // 10. The watched address is indexed like any other; reading its rows is
    //     the application's own SQL.
    let unit = units
        .of(&watched.address.network, &watched.asset)
        .ok_or("ether's unit")?;
    println!("{WATCHED}: watched, fees in {} digits", unit.decimals);

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
    let node = json_rpc::Endpoint::new(url).connect()?;
    let result = node
        .request(json_rpc::Request::once(method, params))
        .await?;
    Ok(result.map_err(json_rpc::CallError::from)?.deserialize()?)
}

/// Sends `amount` of `author`'s asset to `to`, retried while the sender's
/// funds are not indexed yet. Only a definite refusal is retried: it accepted
/// nothing, so trying again cannot send twice.
async fn send_when_funded(
    author: &dyn wallets::Author,
    to: &base::Address,
    amount: &str,
) -> Result<base::TransactionId, AnyError> {
    let amount = amount.parse::<base::Decimal>()?;
    for _ in 0..60 {
        match author.send(to.clone(), amount.clone().into()).await {
            Ok(id) => return Ok(id),
            Err(error) if error.ambiguous_transaction_id.is_none() && !accepted(&error) => {
                tokio::time::sleep(INTERVAL).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err("the sender's funds were never indexed".into())
}

/// Whether a failed send may have reached the chain.
const fn accepted(error: &wallets::WalletError) -> bool {
    !matches!(
        error.kind,
        wallets::WalletErrorKind::Transaction | wallets::WalletErrorKind::Unavailable
    )
}
