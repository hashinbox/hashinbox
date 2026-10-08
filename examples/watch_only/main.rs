//! Watch-only addresses: observe an address on every chain without holding a
//! key for it. `index.watch` takes any `indexing::Holder` — a wallet, or a
//! bare `Subject` of address and asset — and the index stores its history and
//! balance like any wallet's, for the application's own reads. Needs all three
//! local nodes (examples/README.md):
//! `cargo run -p payment-examples --example watch_only`

use std::sync::Arc;

use indexing::{
    Birthday, Checkpoint as _, Driver, Subject, Watching as _,
    sqlite::{Database, Repository},
};
use json_rpc::Client as _;
use payments::config::{Composition, Document, Registry};
use serde_json::{Value, json};
use tokio::sync::watch;
use wallets::WalletRepository as _;
use wallets::{SecretBytes, SubmissionRegistrar, Submissions};

type AnyError = Box<dyn std::error::Error + Send + Sync>;

/// The chains this scenario runs and the node of each.
const CONFIG: &str = include_str!("config.toml");

/// Each chain's asset tag, the id its observed address is filed under, and the
/// seed the address is derived from. The wallet is dropped as soon as its text
/// is read, so this process keeps only the address — exactly what it would
/// have if the address belonged to someone else.
const OBSERVED: [(&str, &str, u8); 3] = [
    ("btc", "btc-observed", 0x31),
    ("eth", "eth-observed", 0x32),
    ("sol", "sol-observed", 0x33),
];

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    // 1. Local testing only: a `solana-test-validator --reset` is a new
    //    cluster every run, so `config.toml` cannot state its genesis hash and
    //    the node is asked for it. A deployment writes the hash it expects.
    let sections: Value = Document::toml(CONFIG).parse()?;
    let url = sections["solana"]["rpc"]["endpoints"][0]
        .as_str()
        .ok_or("[solana] states rpc.endpoints")?;
    let genesis: String = json_rpc::Endpoint::new(url)
        .connect()?
        .request(json_rpc::Request::once("getGenesisHash", json!([])))
        .await?
        .map_err(json_rpc::CallError::from)?
        .deserialize()?;
    let document = Document::toml(CONFIG.replace("<genesis-hash-from-the-node>", &genesis));

    // 2. The chains this application links, one line each. Each parses the
    //    section registered under its name; asset tags are this file's own.
    let mut registry = Registry::<String, Repository>::new();
    registry.register::<chain_bitcoin::Config<String>>("bitcoin");
    registry.register::<chain_ethereum::Config<String>>("ethereum");
    registry.register::<chain_solana::Config<String>>("solana");
    let chains = registry.chains(&document.parse()?)?;

    // 3. Compose. Every node's identity is verified before the store is
    //    opened, then each chain builds its index and factories over it.
    let registrar: Arc<dyn SubmissionRegistrar> = Arc::new(Submissions::new());
    // The file is opened here so its wallet rows can be filed: a watch
    // observes a wallet the store already holds.
    let file = std::path::absolute(".index-data/watch_only.db")?;
    tokio::task::spawn_blocking(|| std::fs::create_dir_all(".index-data")).await??;
    let database = Database::open(file).await?;
    let filed = wallets::storage::sqlite::Repository::new(database.pool().clone());
    let indexing = Repository::new(database.clone());
    let Composition { index, wallets, .. } = chains
        .compose(
            || {
                let database = database.clone();
                async move { Ok::<_, AnyError>(database) }
            },
            |_: &Database| Ok::<_, AnyError>(indexing.clone()),
            &registrar,
        )
        .await?;

    // 4. Observe one address per asset. `parse` turns text into the asset
    //    network's canonical address; a `Subject` is that address and the
    //    asset to read it in, which is all a watch needs.
    for (asset, id, seed) in OBSERVED {
        let asset = asset.to_owned();
        let secret = SecretBytes::new(vec![seed; 32]);
        let text = wallets
            .from_pk(&asset, secret)
            .await?
            .address()
            .value
            .clone();
        let subject = Subject {
            address: wallets.parse(&asset, &text)?,
            asset: wallets.asset(&asset)?.id,
        };
        filed
            .register(wallets::Registration {
                id: id.to_owned(),
                address: subject.address.clone(),
                label: None,
                key: None,
            })
            .await?;
        index.watch(id, &subject, Birthday::Fresh).await?;
        println!("{id}: watching {}", subject.address.value);
    }

    // 5. One sync loop serves a watched address exactly like a wallet's. Wait
    //    until every network has reached its node's tip.
    let (stop, stopped) = watch::channel(false);
    let driver = Driver::new(index.clone());
    let mut readiness = driver.readiness();
    let sync = tokio::spawn(driver.run(stopped));
    let health = readiness.settled().await.ok_or("synchronization stopped")?;
    if let Some(error) = health.networks().iter().find_map(|row| row.failure.clone()) {
        return Err(error.into());
    }

    // 6. Each network's index has inspected the watched addresses up to its
    //    checkpoint; their rows are the application's to read, and need no key.
    for network in index.networks() {
        let checkpoint = index.checkpoint(network).await?;
        println!("{}/{}: {checkpoint:?}", network.chain, network.name);
    }
    stop.send(true)?;
    Ok(sync.await?)
}
