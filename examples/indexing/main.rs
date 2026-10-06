//! Indexing: three chains behind one `indexing::Index`, one
//! `indexing::Supervisor` driving a task per chain, a `Listener` that is told
//! what each commit changed, and what the index holds once every network has
//! reached its tip. No funds move. Needs all three local nodes
//! (examples/README.md): `cargo run -p payment-examples --example indexing`

use std::{collections::BTreeMap, sync::Arc};

use indexing::{
    Birthday, BlockCommit, Checkpoint as _, Listener, Page, Supervisor, SyncReport, Watching as _,
    sqlite::{Database, Repository},
};
use json_rpc::Client as _;
use sdk_config::{Composition, Document, Registry};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use wallets::{SubmissionRegistrar, Submissions, Wallet};

type AnyError = Box<dyn std::error::Error + Send + Sync>;

/// The chains this scenario runs and the node of each.
const CONFIG: &str = include_str!("config.toml");

/// Each chain's asset tag and the one wallet this scenario indexes for it.
const WALLETS: [(&str, &str); 3] = [
    ("btc", "btc-alice"),
    ("eth", "eth-alice"),
    ("sol", "sol-alice"),
];

/// The reaction hook. Synchronous, cannot fail, cannot veto: it hands the
/// commit to a queue and returns. A reaction that must be part of the commit
/// belongs in the repository, which is the only thing that runs inside it.
struct Credits(mpsc::UnboundedSender<BlockCommit>);

impl Listener for Credits {
    fn committed(&self, commit: &BlockCommit) {
        let _ = self.0.send(commit.clone());
    }
}

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
        wallets,
        schedules,
        ..
    } = chains
        .compose(
            &BTreeMap::new(),
            || async {
                let file = std::path::absolute(".index-data/indexing.db")?;
                tokio::task::spawn_blocking(|| std::fs::create_dir_all(".index-data")).await??;
                Ok::<_, AnyError>(Database::open(file).await?)
            },
            |store: &Database| Ok::<_, AnyError>(Repository::new(store.clone())),
            &registrar,
        )
        .await?;

    // 4. The index observes each wallet from the block after its network's
    //    checkpoint. Watch first, keep the key second.
    let mut signers: BTreeMap<&str, Arc<dyn Wallet>> = BTreeMap::new();
    for (asset, id) in WALLETS {
        let wallet = wallets.create(&asset.to_owned()).await?;
        index.watch(id, wallet.as_ref(), Birthday::Fresh).await?;
        signers.insert(id, wallet);
    }

    // 5. Supply a reaction: the hook that says what each commit wrote.
    let (commits, mut committed) = mpsc::unbounded_channel();
    index.listen(Arc::new(Credits(commits)));

    // 6. Run, each chain on the cadence its own section states. Each unit
    //    reads its own watch list on every pass, so a wallet watched while it
    //    runs is picked up without a restart.
    let (stop, stopped) = watch::channel(false);
    let (monitor, mut readiness) = SyncReport::watching(index.networks());
    let sync = tokio::spawn(Supervisor::new(index.clone(), schedules, monitor).run(stopped));
    let health = readiness.settled().await.ok_or("synchronization stopped")?;
    if let Some(error) = health.networks().iter().find_map(|row| row.failure.clone()) {
        return Err(error.into());
    }

    // 7. What the composed index holds: one checkpoint per network, the
    //    addresses it observes there, and each wallet's history.
    for network in index.networks() {
        let checkpoint = index.checkpoint(network).await?;
        println!("{}/{}: {checkpoint:?}", network.chain, network.name);
        for watch in index.watched(network).await? {
            println!("  watches {} from {:?}", watch.address.value, watch.start);
        }
    }
    for (id, wallet) in &signers {
        let page = history.history(wallet.as_ref(), Page::first(10)).await?;
        println!("{id}: {} transaction(s)", page.transactions.len());
    }

    // 8. React, off the sync task.
    stop.send(true)?;
    sync.await?;
    while let Ok(commit) = committed.try_recv() {
        println!(
            "{} committed {}",
            commit.network.chain, commit.block.height.0
        );
    }
    Ok(())
}
