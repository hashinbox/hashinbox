//! Watch-only addresses: observe an address on every chain without holding a
//! key for it. `index.watch` takes any `indexing::Holder` — a wallet, or a
//! bare `Subject` of address and asset — and the index serves its history and
//! balance like any wallet's. Needs all three local nodes (examples/README.md):
//! `cargo run -p payment-examples --example watch_only`

use std::{collections::BTreeMap, sync::Arc};

use indexing::{
    Birthday, Page, Subject, Supervisor, SyncReport, Watching as _,
    sqlite::{Database, Repository},
};
use json_rpc::Client as _;
use sdk_config::{Composition, Document, Registry};
use serde_json::{Value, json};
use tokio::sync::watch;
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
        .connect(&BTreeMap::new())?
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
                let file = std::path::absolute(".index-data/watch_only.db")?;
                tokio::task::spawn_blocking(|| std::fs::create_dir_all(".index-data")).await??;
                Ok::<_, AnyError>(Database::open(file).await?)
            },
            |store: &Database| Ok::<_, AnyError>(Repository::new(store.clone())),
            &registrar,
        )
        .await?;

    // 4. Observe one address per asset. `parse` turns text into the asset
    //    network's canonical address; a `Subject` is that address and the
    //    asset to read it in, which is all a watch needs.
    let mut subjects = Vec::new();
    for (asset, id, seed) in OBSERVED {
        let asset = asset.to_owned();
        let secret = SecretBytes::new(vec![seed; 32]);
        let text = wallets.from(&asset, secret).await?.address().value.clone();
        let subject = Subject {
            address: wallets.parse(&asset, &text)?,
            asset: wallets.asset(&asset)?.id,
        };
        index.watch(id, &subject, Birthday::Fresh).await?;
        println!("{id}: watching {}", subject.address.value);
        let unit = units
            .of(&subject.address.network, &subject.asset)
            .ok_or("a declared unit")?
            .clone();
        subjects.push((id, unit, subject));
    }

    // 5. One sync loop serves a watched address exactly like a wallet's. Wait
    //    until every network has reached its node's tip.
    let (stop, stopped) = watch::channel(false);
    let (monitor, mut readiness) = SyncReport::watching(index.networks());
    let sync = tokio::spawn(Supervisor::new(index.clone(), schedules, monitor).run(stopped));
    let health = readiness.settled().await.ok_or("synchronization stopped")?;
    if let Some(error) = health.networks().iter().find_map(|row| row.failure.clone()) {
        return Err(error.into());
    }

    // 6. History and balance need no key: each is the repository's own
    //    answer, completed by the blocks above its confirmed line.
    for (id, unit, subject) in &subjects {
        let page = history.history(subject, Page::first(10)).await?;
        let balance = balances.balance(subject).await?;
        println!(
            "{id}: {} transaction(s), {} settled, watch_only=true",
            page.transactions.len(),
            unit.display(&balance.amount)?,
        );
    }
    stop.send(true)?;
    Ok(sync.await?)
}
