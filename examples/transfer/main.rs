//! A transfer on every chain at once: fund a sender, run one sync loop, send
//! through the wallet each factory handed back, then read the sender's balance
//! and the receiver's history. Funding is always a local-node call, never SDK
//! API. Needs all three local nodes (examples/README.md):
//! `cargo run -p payment-examples --example transfer`

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use indexing::{
    Birthday, Page, Subject, Supervisor, SyncReport, Watching as _,
    sqlite::{Database, Repository},
};
use json_rpc::Client as _;
use sdk_config::{Composition, Document, Registry};
use serde_json::{Value, json};
use tokio::sync::watch;
use wallets::{SubmissionRegistrar, Submissions, Wallet};

type AnyError = Box<dyn std::error::Error + Send + Sync>;

/// The chains this scenario runs and the node of each.
const CONFIG: &str = include_str!("config.toml");

const INTERVAL: Duration = Duration::from_secs(1);
/// One SOL in lamports.
const AIRDROP: u64 = 1_000_000_000;

/// Each chain's part: its asset tag, the sending wallet, the receiving wallet,
/// and the amount sent in display units.
const TRANSFERS: [(&str, &str, &str, &str); 3] = [
    ("btc", "btc-alice", "btc-bob", "0.5"),
    ("eth", "eth-alice", "eth-bob", "0.01"),
    ("sol", "sol-alice", "sol-bob", "0.1"),
];

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
                let file = std::path::absolute(".index-data/transfer.db")?;
                tokio::task::spawn_blocking(|| std::fs::create_dir_all(".index-data")).await??;
                Ok::<_, AnyError>(Database::open(file).await?)
            },
            |store: &Database| Ok::<_, AnyError>(Repository::new(store.clone())),
            &registrar,
        )
        .await?;

    // 4. Observe new senders before funding: genesis allocations are not
    //    transactions, so the example creates an ordinary indexed deposit.
    let mut signers: BTreeMap<&str, Arc<dyn Wallet>> = BTreeMap::new();
    for (asset, sender, receiver, _) in TRANSFERS {
        let asset = asset.to_owned();
        for id in [sender, receiver] {
            let wallet = wallets.create(&asset).await?;
            index.watch(id, wallet.as_ref(), Birthday::Fresh).await?;
            signers.insert(id, wallet);
        }
    }

    // 5. Funding is a local-network operation and not part of the SDK, so it
    //    uses the node's funded Bitcoin wallet and Solana's airdrop.
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

    // 6. One sync loop over the composed index serves every chain's wallets;
    //    wait until every network has reached its node's tip.
    let (stop, stopped) = watch::channel(false);
    let (monitor, mut readiness) = SyncReport::watching(index.networks());
    let sync = tokio::spawn(Supervisor::new(index.clone(), schedules, monitor).run(stopped));
    let health = readiness.settled().await.ok_or("synchronization stopped")?;
    if let Some(error) = health.networks().iter().find_map(|row| row.failure.clone()) {
        return Err(error.into());
    }

    // 7. One send per chain, once the index has seen the sender's funds:
    //    parse the receiver's address text in the sender's own network, then
    //    send display units. The map hands out an author only for an asset
    //    this system sends.
    for (asset, sender, receiver, amount) in TRANSFERS {
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

    // 8. Only a confirmed transaction is history, so mine bitcoin's block;
    //    Anvil and the validator confirm on their own.
    let mined = json!([depth + 1, coinbase]);
    call(&sections, "bitcoin", "generatetoaddress", mined).await?;
    for (asset, sender, receiver, _) in TRANSFERS {
        let asset = wallets.asset(&asset.to_owned())?.id;
        let to = signers[receiver].address();
        while transactions(history.as_ref(), &asset, to).await?.is_empty() {
            tokio::time::sleep(INTERVAL).await;
        }
        let received = transactions(history.as_ref(), &asset, to).await?.len();
        let settled = balances.balance(signers[sender].as_ref()).await?;
        let unit = units.of(&to.network, &asset).ok_or("a declared unit")?;
        println!(
            "{sender} balance {}; {receiver} has {received} transaction(s)",
            unit.display(&settled.amount)?
        );
    }

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

/// The confirmed transactions of one address in one asset.
async fn transactions(
    history: &dyn indexing::History,
    asset: &base::AssetId,
    address: &base::Address,
) -> Result<Vec<indexing::ObservedTransaction>, AnyError> {
    let subject = Subject {
        address: address.clone(),
        asset: asset.clone(),
    };
    Ok(history
        .history(&subject, Page::first(10))
        .await?
        .transactions)
}
