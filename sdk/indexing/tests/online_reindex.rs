//! The online rescan of one network, on both shipped stores.
//!
//! The quiesced flow `indexing::IndexStore::purge` and `indexing::Control` state:
//! pause the network, purge it, reset its indexer, resume it — while another
//! network in the same repository keeps syncing. What it proves is what the
//! window owes: the network re-derives from its earliest registered birthday,
//! the other network's rows are untouched, a read inside the window is not an
//! authoritative empty answer, and a restart inside the window resumes scanning
//! without a second purge.

mod rows;

use std::{path::PathBuf, sync::Arc, time::Duration};

use base::Decimal;
use indexing::{
    Address, AssetId, BlockHeight, BlockPosition, BlockRef, Budget, ChainName, Config, Control,
    IndexError, IndexErrorKind, IndexStore, IndexedBatch, IndexedOutput, OutputKey, Indexer, Indexers, Indexing, Monitor, MovementId,
    Network, OutputChanges, OutputId, OutputPage, OutputRequest, OutputStore, PurgedRows,
    Report, Repository, RescanError, Schedules, Status, Subject, Supervisor, Transaction,
    TransactionRef, ValueMovement, Watch, WatchProgress, memory, sqlite,
    testing::{Chain, block},
};
use rows::Rows as Reader;
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

/// Every block of `network` through [`TIP`], confirmed, paying each address
/// [`watches`] holds there 10 in every block, as one transaction and one live
/// output.
fn chain(network: &Network) -> Chain {
    let chain = Chain::dense(network.clone(), TIP);
    let owners = watches().into_iter().map(|watch| watch.address);
    for (order, address) in (0_u64..).zip(owners.filter(|at| at.belongs_to(network))) {
        for height in 0..=TIP {
            pay(&chain, BlockHeight(height), order, &address);
        }
    }
    chain
}

/// One transaction of block `height` minting `address` 10 at `order`, and the
/// live output it creates.
fn pay(chain: &Chain, height: BlockHeight, order: u64, address: &Address) {
    let network = &address.network;
    let asset = AssetId::native(network.chain.clone());
    let id = TransactionRef::new(network.clone(), format!("{}-{}", address.value, height.0));
    let movement = ValueMovement::mint(
        MovementId("0".into()),
        asset.clone(),
        Decimal::from(10_u64),
        address.clone(),
    );
    chain.include(
        &block(height.0),
        Transaction {
            id: id.clone(),
            movements: vec![movement],
            fee: None,
            order,
            status: Status::Included {
                block: block(height.0),
            },
        },
    );
    let output = IndexedOutput {
        id: OutputId {
            transaction: id,
            index: 0,
        },
        address: address.clone(),
        asset,
        amount: Decimal::from(10_u64),
        evidence: vec![1],
        created_at: height,
        coinbase: false,
    };
    chain.change_outputs(
        &block(height.0),
        OutputChanges {
            created: vec![output],
            ..OutputChanges::default()
        },
    );
}

/// What a store must answer for this scenario: the minimum, the purge, and the
/// spendable set whose honesty inside the window is the point.
trait Store: Repository + OutputStore + Clone + 'static {}

impl<T> Store for T where T: Repository + OutputStore + Clone + 'static {}

type Unit<R> = Indexer<Chain, Chain, R>;

/// Both networks over one repository, supervised.
struct Running<R: Store> {
    repository: R,
    /// The file's own rows, when the store is a file.
    reader: Option<Reader>,
    reindexed: Arc<Unit<R>>,
    /// The two chains, which record what each block was inspected for: where
    /// a rescan started is invisible from the store.
    reindexed_chain: Chain,
    elsewhere_chain: Chain,
    control: Control,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl<R: Store> Running<R> {
    /// Supervises both networks of `repository`; the rescanned one starts
    /// paused when `paused`, as an application restores a persisted pause.
    fn start(repository: R, reader: Option<Reader>, paused: bool) -> Self {
        let unit = |network: Network| {
            let chain = chain(&network);
            let config = Config::new(&network, Budget::new(100)).expect("sync config");
            let unit = Arc::new(Indexer::new(
                chain.clone(),
                chain.clone(),
                repository.clone(),
                config,
            ));
            (unit, chain)
        };
        let (rescanned, reindexed_chain) = unit(reindexed());
        let (other, elsewhere_chain) = unit(elsewhere());
        let units: Vec<Arc<dyn Indexing>> = vec![rescanned.clone(), other];
        let index = Arc::new(Indexers::of(units).expect("disjoint networks"));
        let (report, _readiness) = Report::watching(index.networks());
        let supervisor = Supervisor::new(
            index,
            Schedules::every(Duration::from_millis(1)),
            report as Arc<dyn Monitor>,
        );
        let control = supervisor.control();
        if paused {
            control.request_pause(&reindexed()).expect("configured");
        }
        let (stop, shutdown) = watch::channel(false);
        Self {
            repository,
            reader,
            reindexed: rescanned,
            reindexed_chain,
            elsewhere_chain,
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

    async fn history(&self, network: &Network, address: &str) -> Option<Vec<String>> {
        let reader = self.reader.as_ref()?;
        let of = subject(network, address);
        Some(reader.history(&of.address, &of.asset, 100).await)
    }

    async fn balance(&self, network: &Network, address: &str) -> Option<Decimal> {
        let reader = self.reader.as_ref()?;
        let of = subject(network, address);
        Some(reader.settled(&of.address, &of.asset).await)
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
    async fn rows(&self, network: &Network, address: &str) -> Snapshot {
        Snapshot {
            checkpoint: self.repository.checkpoint(network).await.expect("tip"),
            progress: self.repository.all(network).await.expect("observations"),
            history: self.history(network, address).await,
            amount: self.balance(network, address).await,
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
        assert_eq!(
            self.repository.checkpoint(&network).await.expect("tip"),
            None,
            "the stored rows claim no line"
        );
        assert!(
            self.history(&network, "early")
                .await
                .is_none_or(|history| history.is_empty())
        );
        let refused = self
            .outputs(&network, "early")
            .await
            .expect_err("an empty spendable set would be a lie, so it is refused");
        assert_eq!(refused.kind, IndexErrorKind::Conflict);
        assert!(refused.retryable, "it clears at the first commit");
    }

    /// The rescan, made of the inspections after the first `since`, began at
    /// the earliest birthday, not at genesis, and each observation was
    /// inspected only from its own.
    ///
    /// Block 2 is read for no address: it is the anchor below the earliest
    /// birthday, the parent the first committed block must connect to.
    fn assert_rederived_from_birthdays(&self, since: usize) {
        let inspected = self.reindexed_chain.inspections().split_off(since);
        let heights: Vec<u64> = inspected.iter().map(|(height, _)| height.0).collect();
        assert_eq!(
            heights,
            (2..=TIP).collect::<Vec<_>>(),
            "read again from the earliest birthday's anchor, once each",
        );
        for (height, addresses) in inspected {
            let addresses: Vec<&str> = addresses.iter().map(|at| at.value.as_str()).collect();
            let expected: &[&str] = match height.0 {
                ..3 => &[],
                3..5 => &["early"],
                _ => &["early", "late"],
            };
            assert_eq!(addresses, expected, "block {}", height.0);
        }
    }
}

/// Everything a read can see of one network; the stored history and total
/// only where the store is a file whose rows can be read.
#[derive(Debug, PartialEq)]
struct Snapshot {
    checkpoint: Option<BlockRef>,
    progress: Vec<WatchProgress>,
    history: Option<Vec<String>>,
    amount: Option<Decimal>,
    outputs: Vec<IndexedOutput>,
}

/// Registers the observations, then indexes both networks to the tip.
async fn indexed<R: Store>(repository: R, reader: Option<Reader>) -> Running<R> {
    // One batch per network: a registration batch is one network's.
    for watch in watches() {
        repository.add(vec![watch]).await.expect("registered");
    }
    let running = Running::start(repository, reader, false);
    running.settles(&reindexed()).await;
    running.settles(&elsewhere()).await;
    running
}

/// Pause → purge → reset → resume, with the other network running throughout.
async fn online_reindex<R: Store>(repository: R, reader: Option<Reader>) {
    let running = indexed(repository, reader).await;
    let early = running.rows(&reindexed(), "early").await;
    let late = running.rows(&reindexed(), "late").await;
    let other = running.rows(&elsewhere(), "other").await;
    assert!(
        early
            .history
            .as_ref()
            .is_none_or(|history| history.len() == 6),
        "blocks 3 through 8"
    );
    assert!(
        late.history
            .as_ref()
            .is_none_or(|history| history.len() == 4),
        "blocks 5 through 8"
    );
    let other_inspections = running.elsewhere_chain.inspections();

    running.control.pause(&reindexed()).await.expect("paused");
    let rescan = running.reindexed_chain.inspections().len();
    let removed = running
        .repository
        .purge(&reindexed())
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

    running.assert_rederived_from_birthdays(rescan);
    assert_eq!(running.rows(&reindexed(), "early").await, early);
    assert_eq!(running.rows(&reindexed(), "late").await, late);
    assert_eq!(running.rows(&elsewhere(), "other").await, other);
    assert_eq!(
        running.elsewhere_chain.inspections(),
        other_inspections,
        "the other network read nothing again",
    );
    running.shut_down().await;
}

/// The same window as one operation of the controller: what it leaves
/// behind is what the steps written out leave.
async fn rescan_operation<R: Store>(repository: R, reader: Option<Reader>) {
    let running = indexed(repository, reader).await;
    let early = running.rows(&reindexed(), "early").await;
    let other = running.rows(&elsewhere(), "other").await;
    let other_inspections = running.elsewhere_chain.inspections();
    let since = running.reindexed_chain.inspections().len();

    let removed = running
        .control
        .rescan(running.reindexed.as_ref(), &running.repository)
        .await
        .expect("rescanned online");
    assert_eq!(removed.history, 10);
    running.settles(&reindexed()).await;

    running.assert_rederived_from_birthdays(since);
    assert_eq!(running.rows(&reindexed(), "early").await, early);
    assert_eq!(running.rows(&elsewhere(), "other").await, other);
    assert_eq!(running.elsewhere_chain.inspections(), other_inspections);
    running.shut_down().await;
}

/// A purge that fails changes nothing and resumes the network; one that
/// "succeeds" without removing the checkpoint stops the window after it, at
/// the reset, and leaves the network paused for another rescan to finish.
async fn a_rescan_reports_where_it_stopped<R: Store>(repository: R, reader: Option<Reader>) {
    let running = indexed(repository, reader).await;
    let refused = running
        .control
        .rescan(running.reindexed.as_ref(), &Purge::Fails)
        .await
        .expect_err("the purge fails");
    assert!(matches!(refused, RescanError::NotStarted(_)), "{refused}");
    running.reindexed_chain.extend_to(TIP + 1);
    let resumed = async {
        while running
            .repository
            .checkpoint(&reindexed())
            .await
            .expect("tip")
            != Some(block(TIP + 1))
        {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), resumed)
        .await
        .expect("the network resumed and indexes on");

    let stopped = running
        .control
        .rescan(running.reindexed.as_ref(), &Purge::Keeps)
        .await
        .expect_err("the reset refuses a network that still has a checkpoint");
    assert!(matches!(stopped, RescanError::Interrupted(_)), "{stopped}");
    assert!(
        stopped.to_string().contains("the network is paused"),
        "{stopped}"
    );
    running.reindexed_chain.extend_to(TIP + 2);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        running
            .repository
            .checkpoint(&reindexed())
            .await
            .expect("tip"),
        Some(block(TIP + 1)),
        "left paused"
    );
    running
        .control
        .rescan(running.reindexed.as_ref(), &running.repository)
        .await
        .expect("a second rescan finishes the window");
    running.shut_down().await;
}

/// A store whose purge does not do its job.
enum Purge {
    /// Refuses, removing nothing.
    Fails,
    /// Answers success and removes nothing.
    Keeps,
}

impl IndexStore for Purge {
    fn checkpoint<'a>(
        &'a self,
        _network: &'a Network,
    ) -> base::BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async { Ok(None) })
    }

    fn commit<'a>(
        &'a self,
        _batch: IndexedBatch,
    ) -> base::BoxFuture<'a, Result<Vec<OutputKey>, IndexError>> {
        Box::pin(async { Err(IndexError::new(IndexErrorKind::Store, "purge-only double", false)) })
    }

    fn purge<'a>(
        &'a self,
        _network: &'a Network,
    ) -> base::BoxFuture<'a, Result<PurgedRows, IndexError>> {
        Box::pin(async move {
            match self {
                Self::Fails => Err(IndexError::new(IndexErrorKind::Store, "disk full", true)),
                Self::Keeps => Ok(PurgedRows::default()),
            }
        })
    }
}

/// A process that stops after the purge and before the resume restarts into
/// the same honest window and finishes by scanning, not by purging again.
async fn restart_inside_the_window<R: Store, F: Future<Output = (R, Option<Reader>)>>(
    open: impl Fn() -> F,
) {
    let (repository, reader) = open().await;
    let running = indexed(repository, reader).await;
    let early = running.rows(&reindexed(), "early").await;
    let other = running.rows(&elsewhere(), "other").await;
    running.control.pause(&reindexed()).await.expect("paused");
    running
        .repository
        .purge(&reindexed())
        .await
        .expect("purged online");
    running.shut_down().await;

    let (repository, reader) = open().await;
    let restarted = Running::start(repository, reader, true);
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

    restarted.assert_rederived_from_birthdays(0);
    assert_eq!(restarted.rows(&reindexed(), "early").await, early);
    assert_eq!(restarted.rows(&elsewhere(), "other").await, other);
    assert!(
        restarted.elsewhere_chain.inspections().is_empty(),
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

    async fn open(&self) -> (sqlite::Repository, Option<Reader>) {
        let database = sqlite::Database::open(&self.path)
            .await
            .expect("sqlite database");
        let reader = Reader::new(database.pool().clone());
        (sqlite::Repository::new(database), Some(reader))
    }
}

mod memory_store {
    use super::{
        a_rescan_reports_where_it_stopped, memory, online_reindex, rescan_operation,
        restart_inside_the_window,
    };

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_rescan_operation_rederives_from_the_earliest_birthday() {
        rescan_operation(memory::Repository::new(), None).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_rescan_says_whether_the_network_is_paused() {
        a_rescan_reports_where_it_stopped(memory::Repository::new(), None).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_purge_reset_resume_rederives_from_the_earliest_birthday() {
        online_reindex(memory::Repository::new(), None).await;
    }

    /// The map outlives the process it stands in for; what is checked is that
    /// a new index over the purged rows needs no second purge.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_restart_inside_the_window_resumes_without_purging_again() {
        let repository = memory::Repository::new();
        restart_inside_the_window(|| std::future::ready((repository.clone(), None))).await;
    }
}

mod sqlite_store {
    use super::{
        File, a_rescan_reports_where_it_stopped, online_reindex, rescan_operation,
        restart_inside_the_window,
    };

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_rescan_operation_rederives_from_the_earliest_birthday() {
        let file = File::new();
        let (repository, reader) = file.open().await;
        rescan_operation(repository, reader).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_rescan_says_whether_the_network_is_paused() {
        let file = File::new();
        let (repository, reader) = file.open().await;
        a_rescan_reports_where_it_stopped(repository, reader).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_purge_reset_resume_rederives_from_the_earliest_birthday() {
        let file = File::new();
        let (repository, reader) = file.open().await;
        online_reindex(repository, reader).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_restart_inside_the_window_resumes_without_purging_again() {
        let file = File::new();
        restart_inside_the_window(|| file.open()).await;
    }
}
