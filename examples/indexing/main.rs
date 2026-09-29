//! Indexing: three chains behind one `indexing::Index`, one
//! `indexing::Supervisor` driving a task per chain, a `Listener` that is told
//! what each commit changed, and what the index holds once every network is
//! `Ready` — the checkpoint of every network, the addresses it watches, and
//! each wallet's history. No funds move. Needs all three local nodes
//! (examples/README.md): `cargo run -p payment-examples --example indexing`

use std::sync::Arc;

use indexing::Supervisor;
use indexing::{
    BlockCommit, Checkpoint as _, Listener, Page, Start, TransactionPage, Watching as _,
};
use local::{AnyError, Asset, Health, Scenario, Signers, Submissions};
use tokio::sync::{mpsc, watch};
use wallets::{Composition, SubmissionRegistrar};

/// Each chain's asset tag and the one wallet this scenario indexes for it.
const WALLETS: [(Asset, &str); 3] = [
    (Asset::Btc, "btc-alice"),
    (Asset::Eth, "eth-alice"),
    (Asset::Sol, "sol-alice"),
];

/// The reaction hook. Synchronous, cannot fail, cannot veto: it hands the
/// commit to a queue and returns. A veto belongs in a `BlockStore`, which is
/// the only thing that runs inside the commit.
struct Credits(mpsc::UnboundedSender<BlockCommit>);

impl Listener for Credits {
    fn committed(&self, commit: &BlockCommit) {
        let _ = self.0.send(commit.clone());
    }
}

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    // 1. Both phases, in order: `config.toml` says what this scenario runs,
    //    `connect` checks that each node serves what its section claims, and
    //    the store opens only once every identity has held. What comes back
    //    is one `Index` over three networks, one factory per asset tag, and
    //    each chain's own polling cadence.
    // Nothing here sends, so nothing registers background work; a scenario
    //    that does — `transfer`, `all_chains` — drains this before it exits.
    let registrar: Arc<dyn SubmissionRegistrar> = Arc::new(Submissions::default());
    let Composition {
        index,
        history,
        wallets,
        schedules,
        ..
    } = Scenario::read("indexing")
        .await?
        .compose(&registrar)
        .await?;

    // 2. The map holds factories only; it makes a wallet and hands it over.
    //    The index is what observes it, from the block after the checkpoint
    //    its watch was published at, and its row commits before the key is
    //    kept (`docs/design/030 §8 (ex-029)` section 3.3).
    let mut signers = Signers::default();
    for (asset, id) in WALLETS {
        let wallet = wallets.create(&asset).await?;
        index.watch(id, wallet.as_ref(), Start::Fresh).await?;
        signers.keep(id, wallet);
    }

    // 3. Supply a reaction. The bundled SQLite store is already wired into each
    //    chain's indexer; this is the hook that says what it just wrote.
    let (commits, mut committed) = mpsc::unbounded_channel();
    index.listen(Arc::new(Credits(commits)));

    // 4. Run, each chain on the cadence its own section states. The
    //    supervisor needs no selection argument: each unit reads its own watch
    //    list on every pass, so a wallet watched while it runs is picked up
    //    without restarting it.
    let (stop, stopped) = watch::channel(false);
    let (monitor, mut readiness) = Health::watching(index.networks());
    let sync = tokio::spawn(Supervisor::new(index.clone(), schedules, monitor).run(stopped));
    readiness.ready().await?;

    // 5. What the composed index holds: one checkpoint per network, and the
    //    addresses it observes there.
    for network in index.networks() {
        let checkpoint = index.checkpoint(network).await?;
        println!("{}/{}: {checkpoint:?}", network.chain, network.name);
        for watch in index.watched(network).await? {
            println!("  watches {} from {:?}", watch.address.value, watch.start);
        }
    }
    for (_, id) in WALLETS {
        let page = page_of(history.as_ref(), signers.get(id)?.as_ref()).await?;
        println!("{id}: {} transaction(s)", page.transactions.len());
    }

    // 6. React, off the sync task.
    stop.send(true)?;
    sync.await?;
    while let Ok(commit) = committed.try_recv() {
        println!(
            "{} at {}: {} transaction(s), {} new output(s)",
            commit.network.chain,
            commit.block.height.0,
            commit.transactions.len(),
            commit.outputs.created.len()
        );
    }
    Ok(())
}

/// One page of a holder's history: the subject is the holder itself, and the
/// page is the only other question. Amounts arrive atomic; the unit each
/// asset's own chain declares is what scales one for display, and nothing
/// here prints one.
async fn page_of(
    history: &dyn indexing::History,
    holder: &dyn indexing::Holder,
) -> Result<TransactionPage, AnyError> {
    Ok(history.history(holder, Page::first(10)).await?)
}
