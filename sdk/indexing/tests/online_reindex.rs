//! The online rescan of one network, on both shipped stores.
//!
//! The quiesced flow `indexing::Purging` and `indexing::SyncControl` state:
//! pause the network, purge it, reset its indexer, resume it — while another
//! network in the same repository keeps syncing. What it proves is what the
//! window owes: the network re-derives from its earliest registered birthday,
//! the other network's rows are untouched, a read inside the window is not an
//! authoritative empty answer, and a restart inside the window resumes scanning
//! without a second purge.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use base::Decimal;
use indexing::{
    Address, AssetId, Balance, BalanceQuery, BalanceStore, BlockHash, BlockHeight,
    BlockInterpreter, BlockParent, BlockPosition, BlockRef, BlockSource, BoxFuture, CanonicalPage,
    ChainName, ChainTips, Completeness, ConfirmedHistory, FeeSettlement, HistoryQuery, Index,
    IndexError, IndexErrorKind, IndexedOutput, Indexer, Indexing, InterpretedBlock, MovementId,
    NativeBlock, Network, ObservationDraft, ObservationDraftStatus, OutputChanges, OutputId,
    OutputPage, OutputRequest, OutputStore, Page, Purging, Repository, Schedules, SourceError,
    Subject, Supervisor, SyncConfig, SyncControl, SyncMonitor, SyncPolicy, SyncReport,
    TransactionRef, ValueMovement, Watch, WatchProgress, memory, sqlite,
};
use tempfile::TempDir;
use tokio::{sync::watch, task::JoinHandle};

/// The produced and confirmed tip of both chains: nothing is pending, so every
/// fact a read can see is a stored one.
const TIP: u64 = 8;

fn reindexed() -> Network {
    Network::new(ChainName::new("online"), "reindexed")
}

fn elsewhere() -> Network {
    Network::new(ChainName::new("online"), "elsewhere")
}

fn block(height: u64) -> BlockRef {
    BlockRef {
        position: BlockPosition(height),
        height: BlockHeight(height),
        hash: BlockHash(height.to_be_bytes().to_vec()),
        parent: height.checked_sub(1).map(|parent| BlockParent {
            position: BlockPosition(parent),
            hash: BlockHash(parent.to_be_bytes().to_vec()),
        }),
        timestamp: None,
    }
}

fn subject(network: &Network, address: &str) -> Subject {
    Subject {
        address: Address::new(network.clone(), address),
        asset: AssetId::native(network.chain.clone()),
    }
}

/// The observations of both networks. The rescanned one has two birthdays, so
/// "from the earliest" is distinguishable from "from genesis" and from "from
/// each watch's own".
fn watches() -> Vec<Watch> {
    let watch = |network: &Network, address: &str, start: u64| {
        let subject = subject(network, address);
        Watch {
            id: format!("{}-{address}", network.name),
            address: subject.address,
            asset: subject.asset,
            start: BlockPosition(start),
        }
    };
    vec![
        watch(&reindexed(), "early", 3),
        watch(&reindexed(), "late", 5),
        watch(&elsewhere(), "other", 1),
    ]
}

#[derive(Clone, Debug)]
struct Block(BlockRef);

impl NativeBlock for Block {
    fn block_ref(&self) -> BlockRef {
        self.0.clone()
    }
}

/// Every block through [`TIP`], confirmed.
#[derive(Clone)]
struct Source;

impl ChainTips for Source {
    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        Box::pin(async { Ok(block(TIP)) })
    }

    fn confirmed_tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        Box::pin(async { Ok(block(TIP)) })
    }
}

impl BlockSource for Source {
    type Block = Block;

    fn blocks<'a>(
        &'a self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Self::Block>, SourceError>> {
        let blocks = (start.0..=end.0.min(TIP))
            .take(limit)
            .map(|height| Block(block(height)))
            .collect();
        Box::pin(async move { Ok(blocks) })
    }

    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, SourceError>> {
        let found = (position.0 <= TIP).then(|| block(position.0));
        Box::pin(async move { Ok(found) })
    }
}

/// Pays every selected address 10 in every block it is handed, as one
/// transaction and one live output, and records which addresses each block was
/// inspected for: where a rescan started is invisible from the store.
/// One block's height, and the addresses it was inspected for.
type Inspected = (u64, Vec<String>);

#[derive(Clone)]
struct Interpreter {
    network: Network,
    inspected: Arc<Mutex<Vec<Inspected>>>,
}

impl Interpreter {
    fn new(network: Network) -> Self {
        Self {
            network,
            inspected: Arc::default(),
        }
    }

    fn inspected(&self) -> Vec<Inspected> {
        self.inspected.lock().expect("inspections").clone()
    }

    fn forget(&self) {
        self.inspected.lock().expect("inspections").clear();
    }
}

impl BlockInterpreter for Interpreter {
    type Block = Block;

    fn settlement(&self) -> FeeSettlement {
        FeeSettlement::Charged
    }

    fn inspect(
        &self,
        block: &Self::Block,
        addresses: &[Address],
    ) -> Result<InterpretedBlock, IndexError> {
        let reference = block.block_ref();
        let height = reference.height;
        self.inspected.lock().expect("inspections").push((
            height.0,
            addresses.iter().map(|at| at.value.clone()).collect(),
        ));
        let asset = AssetId::native(self.network.chain.clone());
        let mut transactions = Vec::new();
        let mut created = Vec::new();
        for (order, address) in (0_u64..).zip(addresses) {
            let id = TransactionRef::new(
                self.network.clone(),
                format!("{}-{}", address.value, height.0),
            );
            transactions.push(ObservationDraft {
                transaction: indexing::Transaction {
                    transaction_id: id.clone(),
                    movements: vec![ValueMovement::mint(
                        MovementId("0".into()),
                        asset.clone(),
                        Decimal::from(10_u64),
                        address.clone(),
                    )],
                    fee: None,
                },
                transaction_order: order,
                status: ObservationDraftStatus::Included,
            });
            created.push(IndexedOutput {
                id: OutputId {
                    transaction: id,
                    index: 0,
                },
                address: address.clone(),
                asset: asset.clone(),
                amount: Decimal::from(10_u64),
                evidence: vec![1],
                created_at: height,
                coinbase: false,
            });
        }
        Ok(InterpretedBlock {
            outputs: OutputChanges {
                created,
                ..OutputChanges::default()
            },
            ..InterpretedBlock::new(&self.network, reference, transactions)?
        })
    }
}

/// What a store must answer for this scenario: the minimum, the purge, and the
/// three reads whose honesty inside the window is the point.
trait Store:
    Repository + Purging + OutputStore + ConfirmedHistory + BalanceStore + Clone + 'static
{
}

impl<T> Store for T where
    T: Repository + Purging + OutputStore + ConfirmedHistory + BalanceStore + Clone + 'static
{
}

type Unit<R> = Indexer<Source, Interpreter, R>;

/// Both networks over one repository, supervised.
struct Running<R: Store> {
    repository: R,
    reindexed: Arc<Unit<R>>,
    reindexed_log: Interpreter,
    elsewhere_log: Interpreter,
    control: SyncControl,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl<R: Store> Running<R> {
    /// Supervises both networks of `repository`; the rescanned one starts
    /// paused when `paused`, as an application restores a persisted pause.
    fn start(repository: R, paused: bool) -> Self {
        let unit = |network: Network| {
            let interpreter = Interpreter::new(network.clone());
            let config = SyncConfig::new(&network, SyncPolicy::new(100)).expect("sync config");
            let unit = Arc::new(Indexer::new(
                Source,
                interpreter.clone(),
                repository.clone(),
                config,
            ));
            (unit, interpreter)
        };
        let (rescanned, reindexed_log) = unit(reindexed());
        let (other, elsewhere_log) = unit(elsewhere());
        let units: Vec<Arc<dyn Indexing>> = vec![rescanned.clone(), other];
        let index = Arc::new(Index::of(units).expect("disjoint networks"));
        let (report, _readiness) = SyncReport::watching(index.networks());
        let supervisor = Supervisor::new(
            index,
            Schedules::every(Duration::from_millis(1)),
            report as Arc<dyn SyncMonitor>,
        );
        let control = supervisor.control();
        if paused {
            control.request_pause(&reindexed()).expect("configured");
        }
        let (stop, shutdown) = watch::channel(false);
        Self {
            repository,
            reindexed: rescanned,
            reindexed_log,
            elsewhere_log,
            control,
            stop,
            task: tokio::spawn(supervisor.run(shutdown)),
        }
    }

    /// Waits until `network` is committed through [`TIP`] with every
    /// observation's progress there too.
    async fn settles(&self, network: &Network) {
        let settled = async {
            loop {
                let checkpoint = self.repository.checkpoint(network).await.expect("tip");
                let observed = self.repository.all(network).await.expect("observations");
                let line = Some(block(TIP));
                if checkpoint == line && observed.iter().all(|row| row.indexed_through == line) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), settled)
            .await
            .unwrap_or_else(|_| panic!("{} never reached the tip", network.name));
    }

    async fn shut_down(self) {
        self.stop.send_replace(true);
        self.task.await.expect("the supervisor ends cleanly");
    }

    async fn history(&self, network: &Network, address: &str) -> CanonicalPage {
        let query = HistoryQuery::new(&subject(network, address), Page::first(100));
        self.repository.history(query).await.expect("history")
    }

    async fn balance(&self, network: &Network, address: &str) -> Balance {
        let query = BalanceQuery::new(&subject(network, address));
        self.repository.balance(query).await.expect("balance")
    }

    async fn outputs(&self, network: &Network, address: &str) -> Result<OutputPage, IndexError> {
        OutputStore::list(
            &self.repository,
            OutputRequest {
                network: network.clone(),
                address: Address::new(network.clone(), address),
                after: None,
                limit: 100,
            },
        )
        .await
    }

    /// Everything a read can see of one network, to compare across the window.
    async fn rows(&self, network: &Network, address: &str) -> Rows {
        let history = self.history(network, address).await;
        Rows {
            checkpoint: self.repository.checkpoint(network).await.expect("tip"),
            progress: self.repository.all(network).await.expect("observations"),
            history: ids(&history),
            amount: self.balance(network, address).await.amount,
            outputs: self
                .outputs(network, address)
                .await
                .expect("a settled network's outputs")
                .outputs,
        }
    }

    /// The reads inside the window: nothing is served as authoritative.
    async fn assert_window(&self) {
        let network = reindexed();
        let page = self.history(&network, "early").await;
        assert!(page.transactions.is_empty());
        assert_eq!(page.checkpoint, None, "the page claims no line");
        assert_eq!(page.completeness, Completeness::Incomplete);
        let balance = self.balance(&network, "early").await;
        assert_eq!(balance.observed_at, None);
        assert_eq!(balance.completeness, Completeness::Incomplete);
        let refused = self
            .outputs(&network, "early")
            .await
            .expect_err("an empty spendable set would be a lie, so it is refused");
        assert_eq!(refused.kind, IndexErrorKind::Conflict);
        assert!(refused.retryable, "it clears at the first commit");
    }

    /// The rescan began at the earliest birthday, not at genesis, and each
    /// observation was inspected only from its own.
    ///
    /// Block 2 is read for no address: it is the anchor below the earliest
    /// birthday, the parent the first committed block must connect to.
    fn assert_rederived_from_birthdays(&self) {
        let inspected = self.reindexed_log.inspected();
        let heights: Vec<u64> = inspected.iter().map(|(height, _)| *height).collect();
        assert_eq!(
            heights,
            (2..=TIP).collect::<Vec<_>>(),
            "read again from the earliest birthday's anchor, once each",
        );
        for (height, addresses) in inspected {
            let expected: &[&str] = match height {
                ..3 => &[],
                3..5 => &["early"],
                _ => &["early", "late"],
            };
            assert_eq!(addresses, expected, "block {height}");
        }
    }
}

#[derive(Debug, PartialEq)]
struct Rows {
    checkpoint: Option<BlockRef>,
    progress: Vec<WatchProgress>,
    history: Vec<String>,
    amount: Decimal,
    outputs: Vec<IndexedOutput>,
}

fn ids(page: &CanonicalPage) -> Vec<String> {
    page.transactions
        .iter()
        .map(|entry| entry.transaction.transaction_id.id.to_string())
        .collect()
}

/// Registers the observations, then indexes both networks to the tip.
async fn indexed<R: Store>(repository: R) -> Running<R> {
    // One batch per network: a registration batch is one network's.
    for watch in watches() {
        repository.add(vec![watch]).await.expect("registered");
    }
    let running = Running::start(repository, false);
    running.settles(&reindexed()).await;
    running.settles(&elsewhere()).await;
    running
}

/// Pause → purge → reset → resume, with the other network running throughout.
async fn online_reindex<R: Store>(repository: R) {
    let running = indexed(repository).await;
    let early = running.rows(&reindexed(), "early").await;
    let late = running.rows(&reindexed(), "late").await;
    let other = running.rows(&elsewhere(), "other").await;
    assert_eq!(early.history.len(), 6, "blocks 3 through 8");
    assert_eq!(late.history.len(), 4, "blocks 5 through 8");
    let other_inspections = running.elsewhere_log.inspected();

    running.control.pause(&reindexed()).await.expect("paused");
    running.reindexed_log.forget();
    let removed = running
        .repository
        .purge_network(&reindexed())
        .await
        .expect("purged online");
    assert_eq!(removed.history, 10);
    running.assert_window().await;
    assert_eq!(
        running.rows(&elsewhere(), "other").await,
        other,
        "the other network is read normally inside the window",
    );

    running
        .reindexed
        .reset()
        .await
        .expect("reset after the purge");
    running
        .control
        .discard_progress(&reindexed())
        .await
        .expect("reported progress cleared");
    running.assert_window().await;
    running.control.resume(&reindexed()).expect("resumed");
    running.settles(&reindexed()).await;

    running.assert_rederived_from_birthdays();
    assert_eq!(running.rows(&reindexed(), "early").await, early);
    assert_eq!(running.rows(&reindexed(), "late").await, late);
    assert_eq!(running.rows(&elsewhere(), "other").await, other);
    assert_eq!(
        running.elsewhere_log.inspected(),
        other_inspections,
        "the other network read nothing again",
    );
    running.shut_down().await;
}

/// A process that stops after the purge and before the resume restarts into
/// the same honest window and finishes by scanning, not by purging again.
async fn restart_inside_the_window<R: Store>(open: impl Fn() -> R) {
    let running = indexed(open()).await;
    let early = running.rows(&reindexed(), "early").await;
    let other = running.rows(&elsewhere(), "other").await;
    running.control.pause(&reindexed()).await.expect("paused");
    running
        .repository
        .purge_network(&reindexed())
        .await
        .expect("purged online");
    running.shut_down().await;

    let restarted = Running::start(open(), true);
    restarted.assert_window().await;
    assert!(
        restarted
            .repository
            .all(&reindexed())
            .await
            .expect("observations")
            .iter()
            .all(|row| row.indexed_through.is_none()),
        "the restart finds the observations the purge kept, without progress",
    );
    restarted
        .reindexed
        .reset()
        .await
        .expect("replaying the reset step is harmless");
    restarted.control.resume(&reindexed()).expect("resumed");
    restarted.settles(&reindexed()).await;

    restarted.assert_rederived_from_birthdays();
    assert_eq!(restarted.rows(&reindexed(), "early").await, early);
    assert_eq!(restarted.rows(&elsewhere(), "other").await, other);
    assert!(
        restarted.elsewhere_log.inspected().is_empty(),
        "the other network resumed at its own line",
    );
    restarted.shut_down().await;
}

/// One SQLite file, reopened as a restarted process opens it.
struct File {
    _directory: TempDir,
    path: PathBuf,
}

impl File {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("index.sqlite");
        Self {
            _directory: directory,
            path,
        }
    }

    fn open(&self) -> sqlite::Repository {
        sqlite::Repository::new(sqlite::Database::open(&self.path).expect("sqlite database"))
    }
}

mod memory_store {
    use super::{memory, online_reindex, restart_inside_the_window};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_purge_reset_resume_rederives_from_the_earliest_birthday() {
        online_reindex(memory::Repository::new()).await;
    }

    /// The map outlives the process it stands in for; what is checked is that
    /// a new index over the purged rows needs no second purge.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_restart_inside_the_window_resumes_without_purging_again() {
        let repository = memory::Repository::new();
        restart_inside_the_window(|| repository.clone()).await;
    }
}

mod sqlite_store {
    use super::{File, online_reindex, restart_inside_the_window};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_purge_reset_resume_rederives_from_the_earliest_birthday() {
        online_reindex(File::new().open()).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_restart_inside_the_window_resumes_without_purging_again() {
        let file = File::new();
        restart_inside_the_window(|| file.open()).await;
    }
}
