//! Contract coverage for `Index`: assembly over one store, network routing of
//! reads and watches, and
//! the status report it accepts from each child.

use std::sync::{Arc, Mutex};

use futures_executor::block_on;
use indexing::{
    Address, Balance, Balances, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef,
    BoxFuture, ChainName, Checkpoint, Completeness, History, Holder, Index, IndexError,
    IndexErrorKind, Indexing, Network, Page, PendingValue, Start, Subject, SyncPhase, SyncStatus,
    TransactionPage, Watch, Watching,
};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Checkpoint(Network),
    History(Box<Subject>, Page),
    Balance(Box<Subject>),
    Watch(Box<Watch>),
    Unwatch(String),
    Watched(Network),
    Cancel(Network),
    Sync,
}

struct Probe {
    network: Network,
    checkpoint: BlockRef,
    calls: Mutex<Vec<Call>>,
}

impl Probe {
    fn new(network: Network, height: u64) -> Self {
        Self {
            network,
            checkpoint: block(height),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("probe calls").clone()
    }

    fn record(&self, call: Call) {
        self.calls.lock().expect("probe calls").push(call);
    }
}

impl Checkpoint for Probe {
    fn checkpoint<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        self.record(Call::Checkpoint(network.clone()));
        let checkpoint = self.checkpoint.clone();
        Box::pin(async move { Ok(Some(checkpoint)) })
    }
}

impl History for Probe {
    fn history<'a>(
        &'a self,
        of: &'a dyn Holder,
        page: Page,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
        self.record(Call::History(Box::new(recorded(of)), page));
        let checkpoint = self.checkpoint.clone();
        Box::pin(async move {
            Ok(TransactionPage {
                checkpoint: Some(checkpoint),
                transactions: Vec::new(),
                pending: Vec::new(),
                next: None,
                completeness: indexing::Completeness::Incomplete,
            })
        })
    }
}

impl Balances for Probe {
    fn balance<'a>(&'a self, of: &'a dyn Holder) -> BoxFuture<'a, Result<Balance, IndexError>> {
        self.record(Call::Balance(Box::new(recorded(of))));
        let checkpoint = self.checkpoint.clone();
        Box::pin(async move {
            Ok(Balance {
                amount: indexing::Decimal::zero(),
                observed_at: Some(checkpoint),
                pending: PendingValue::none(),
                completeness: Completeness::Incomplete,
            })
        })
    }
}

impl Indexing for Probe {
    fn network(&self) -> &Network {
        &self.network
    }

    fn listen(&self, _listener: Arc<dyn indexing::Listener>) {}

    fn sync<'a>(&'a self) -> BoxFuture<'a, Result<SyncStatus, IndexError>> {
        self.record(Call::Sync);
        let status = SyncStatus {
            network: self.network.clone(),
            checkpoint: Some(self.checkpoint.clone()),
            pending_tip: None,
            observed_tip: Some(self.checkpoint.clone()),
            phase: SyncPhase::Ready,
            unknown_spends: Vec::new(),
            backfill: None,
        };
        Box::pin(async move { Ok(status) })
    }
}

impl Watching for Probe {
    fn watch<'a>(
        &'a self,
        id: &'a str,
        subject: &'a dyn Holder,
        start: Start,
    ) -> BoxFuture<'a, Result<Watch, IndexError>> {
        let watch = Watch {
            id: id.to_owned(),
            address: subject.address().clone(),
            asset: subject.asset().clone(),
            start: match start {
                Start::At(position) => position,
                Start::Fresh => self.checkpoint.position,
            },
        };
        self.record(Call::Watch(Box::new(watch.clone())));
        Box::pin(async move { Ok(watch) })
    }

    /// A probe holds one address per id, named by the id itself, so a routed
    /// unwatch can be told apart from one that reached the wrong unit.
    fn unwatch<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Watch>, IndexError>> {
        self.record(Call::Unwatch(id.to_owned()));
        let network = self.network.clone();
        let found = id.starts_with(network.chain.as_str());
        let watch = Watch {
            id: id.to_owned(),
            address: Address::new(network.clone(), id),
            asset: indexing::AssetId::native(network.chain),
            start: self.checkpoint.position,
        };
        Box::pin(async move { Ok(found.then_some(watch)) })
    }

    fn watched<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
        self.record(Call::Watched(network.clone()));
        Box::pin(async { Ok(Vec::new()) })
    }
}

impl indexing::Backfilling for Probe {
    fn cancel<'a>(&'a self, network: &'a Network) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
        self.record(Call::Cancel(network.clone()));
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// The holder a probe recorded, as a comparable value.
fn recorded(of: &dyn Holder) -> Subject {
    Subject {
        address: of.address().clone(),
        asset: of.asset().clone(),
    }
}

/// The native-asset subject of one address.
fn subject(address: &Address) -> Subject {
    Subject {
        address: address.clone(),
        asset: indexing::AssetId::native(address.network.chain.clone()),
    }
}

fn network(chain: &str) -> Network {
    Network {
        chain: ChainName::new(chain),
        name: "testnet".into(),
    }
}

fn address(network: &Network, value: &str) -> Address {
    Address {
        network: network.clone(),
        value: value.into(),
    }
}

fn block(height: u64) -> BlockRef {
    BlockRef {
        position: BlockPosition(height),
        height: BlockHeight(height),
        hash: BlockHash(vec![height as u8]),
        parent: height.checked_sub(1).map(|value| BlockParent {
            position: BlockPosition(value),
            hash: BlockHash(vec![value as u8]),
        }),
        timestamp: None,
    }
}

/// Every read and watch an index answers, over whatever answers them: one
/// chain's own indexer, or a composition of one.
///
/// The two differ only in how many networks they name, which is exactly what
/// this cannot ask them: a child answers [`Indexing::network`] and one
/// `SyncStatus`, an `Index` answers `networks()` and a vector of them, and the
/// test below states each half where it belongs.
///
/// **No query is in here**, and that is the contract rather than an omission: a
/// balance and a page are the repository's own answers, held per network by the
/// consumer, so neither an indexer nor a composition of them is asked for one.
fn exercise_reads<T>(indexer: &T, network: &Network, height: u64)
where
    T: Checkpoint + Watching + ?Sized,
{
    assert_eq!(
        block_on(indexer.checkpoint(network)).expect("checkpoint"),
        Some(block(height))
    );

    let owner = address(network, "owner");
    let watched =
        block_on(indexer.watch("owner", &subject(&owner), Start::At(BlockPosition(height))))
            .expect("watch");
    assert_eq!(watched.address, owner);
    assert_eq!(watched.start, BlockPosition(height));
    assert!(
        block_on(indexer.watched(network))
            .expect("watched")
            .is_empty()
    );
}

/// One chain's indexer and a composition of it answer the same reads, and each
/// reports its own shape of progress: one network and one status for the child,
/// every network and one status per unit for the composition.
#[test]
fn one_chain_and_composer_share_the_read_contract() {
    let single_network = network("single-chain");
    let single: &dyn Indexing = &Probe::new(single_network.clone(), 4);
    exercise_reads(single, &single_network, 4);
    assert_eq!(single.network(), &single_network);
    assert_eq!(
        block_on(single.sync())
            .expect("one child, one status")
            .network,
        single_network
    );

    let composed_network = network("composed-chain");
    let index = Index::of(vec![
        Arc::new(Probe::new(composed_network.clone(), 7)) as Arc<dyn Indexing>
    ])
    .expect("one chain composes");
    exercise_reads(&index, &composed_network, 7);
    assert_eq!(index.networks(), std::slice::from_ref(&composed_network));
    assert_eq!(
        block_on(index.sync())
            .expect("one unit, one status")
            .into_iter()
            .map(|status| status.network)
            .collect::<Vec<_>>(),
        [composed_network]
    );
}

#[test]
fn routes_each_networked_operation_to_its_indexer() {
    let first_network = network("first-chain");
    let second_network = network("second-chain");
    let first = Arc::new(Probe::new(first_network.clone(), 4));
    let second = Arc::new(Probe::new(second_network.clone(), 7));
    let index = Index::of(vec![
        first.clone() as Arc<dyn Indexing>,
        second.clone() as Arc<dyn Indexing>,
    ])
    .expect("disjoint networks compose");

    let checkpoint = block_on(index.checkpoint(&first_network)).expect("checkpoint");
    assert_eq!(checkpoint, Some(block(4)));

    let owner = address(&second_network, "owner");
    let watched = block_on(index.watch("owner", &subject(&owner), Start::Fresh)).expect("watch");
    assert_eq!(watched.address, owner);

    assert_eq!(first.calls(), vec![Call::Checkpoint(first_network)]);
    assert_eq!(second.calls(), vec![Call::Watch(Box::new(watched))]);
}

/// Why an assembly refused, or a panic naming what was expected. `Index` is not
/// `Debug`, so `expect_err` cannot be used on it.
fn refusal<C: ?Sized>(built: Result<Index<C>, IndexError>, expected: &str) -> IndexError {
    match built {
        Ok(_) => panic!("{expected}"),
        Err(error) => error,
    }
}

/// **The assembly flow `docs/design/030 §8 (ex-032)` describes, as one test.** One store, three
/// chains, and a validation that launches nothing.
///
/// What it pins is what the flow is *for*: each chain is handed the same store by
/// the index rather than by the caller, so a third party writes the store once;
/// and `build` is where a duplicate network or a chain that refused to compose is
/// reported, before anything has been driven.
#[test]
fn an_assembly_hands_one_store_to_every_chain_and_launches_nothing() {
    /// The one thing a chain needs of a store here: that it arrived.
    #[derive(Clone)]
    struct Store(Arc<Mutex<Vec<String>>>);

    /// One chain's constructor, which records that the store reached it.
    fn chain(name: &'static str) -> impl FnOnce(Store) -> Result<Arc<dyn Indexing>, IndexError> {
        move |store: Store| {
            store.0.lock().expect("handed to").push(name.to_owned());
            Ok(Arc::new(Probe::new(network(name), 1)) as Arc<dyn Indexing>)
        }
    }

    /// The flow as a caller writes it: one `?` per chain, and no builder that
    /// remembers a failure to report later.
    fn three(store: Store) -> Result<Index<dyn Indexing>, IndexError> {
        Index::over(store)
            .chain(chain("bitcoin"))?
            .chain(chain("ethereum"))?
            .chain(chain("solana"))?
            .build()
    }

    fn duplicated(store: Store) -> Result<Index<dyn Indexing>, IndexError> {
        Index::over(store)
            .chain(chain("bitcoin"))?
            .chain(chain("bitcoin"))?
            .build()
    }

    fn refusing(store: Store) -> Result<Index<dyn Indexing>, IndexError> {
        Index::over(store)
            .chain(|_| Err("the node serves another chain id"))?
            .chain(chain("bitcoin"))?
            .build()
    }

    let store = Store(Arc::new(Mutex::new(Vec::new())));
    let index = match three(store.clone()) {
        Ok(index) => index,
        Err(error) => panic!("three disjoint chains over one store: {error}"),
    };

    assert_eq!(
        *store.0.lock().expect("handed to"),
        ["bitcoin", "ethereum", "solana"],
        "every chain was handed the one store, in the order it was added",
    );
    assert_eq!(
        index.networks(),
        [network("bitcoin"), network("ethereum"), network("solana")],
        "and the index answers for all three",
    );
    assert_eq!(
        index
            .units()
            .iter()
            .map(|unit| unit.network().clone())
            .collect::<Vec<_>>(),
        index.networks(),
        "each chain is still its own independently drivable unit, of one network",
    );

    // Two chains claiming one network is refused at `build`, not at the first
    // pass; a chain that refuses to compose is refused at that chain.
    let duplicate = refusal(duplicated(store.clone()), "two chains, one network");
    assert_eq!(duplicate.kind, IndexErrorKind::Conflict);

    let handed = store.0.lock().expect("handed to").len();
    let refused = refusal(
        refusing(store.clone()),
        "a chain that could not be composed",
    );
    assert_eq!(refused.kind, IndexErrorKind::InvalidRequest);
    assert!(
        refused.message.contains("the node serves another chain id"),
        "carrying the chain's own reason: {}",
        refused.message
    );
    assert_eq!(
        store.0.lock().expect("handed to").len(),
        handed,
        "and nothing after the refusal was constructed at all",
    );
}

#[test]
fn rejects_an_empty_index() {
    let result = Index::<dyn Indexing>::of(Vec::new());
    let error = match result {
        Ok(_) => panic!("an index of nothing must not report readiness"),
        Err(error) => error,
    };

    assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
}

#[test]
fn rejects_duplicate_networks() {
    let shared_network = network("same-chain");
    let result = Index::of(vec![
        Arc::new(Probe::new(shared_network.clone(), 1)) as Arc<dyn Indexing>,
        Arc::new(Probe::new(shared_network, 2)) as Arc<dyn Indexing>,
    ]);

    let error = match result {
        Ok(_) => panic!("duplicate networks must not compose"),
        Err(error) => error,
    };
    assert_eq!(error.kind, IndexErrorKind::Conflict);
}

#[test]
fn rejects_operations_for_an_unconfigured_network() {
    let configured = network("configured-chain");
    let missing = network("missing-chain");
    let index = Index::of(vec![
        Arc::new(Probe::new(configured, 1)) as Arc<dyn Indexing>
    ])
    .expect("one network composes");

    let checkpoint_error = block_on(index.checkpoint(&missing)).expect_err("missing checkpoint");
    let watch_error =
        block_on(index.watch("owner", &subject(&address(&missing, "owner")), Start::Fresh))
            .expect_err("missing watch network");
    let watched_error = block_on(index.watched(&missing)).expect_err("missing watched network");

    assert_eq!(checkpoint_error.kind, IndexErrorKind::NetworkMismatch);
    assert_eq!(watch_error.kind, IndexErrorKind::NetworkMismatch);
    assert_eq!(watched_error.kind, IndexErrorKind::NetworkMismatch);
}

/// Every watch goes to the unit that owns its address's network, and one pass
/// advances every unit in construction order.
#[test]
fn routes_each_watch_and_combines_statuses_from_every_indexer() {
    let first_network = network("first-chain");
    let second_network = network("second-chain");
    let idle_network = network("idle-chain");
    let first = Arc::new(Probe::new(first_network.clone(), 4));
    let second = Arc::new(Probe::new(second_network.clone(), 7));
    let idle = Arc::new(Probe::new(idle_network.clone(), 9));
    let index = Index::of(vec![
        first.clone() as Arc<dyn Indexing>,
        second.clone() as Arc<dyn Indexing>,
        idle.clone() as Arc<dyn Indexing>,
    ])
    .expect("disjoint networks compose");

    let first_owner = address(&first_network, "first-owner");
    let second_owner = address(&second_network, "second-owner");
    for (id, owner, start) in [
        ("second", &second_owner, BlockPosition(5)),
        ("first", &first_owner, BlockPosition(2)),
    ] {
        block_on(index.watch(id, &subject(owner), Start::At(start))).expect("watch");
    }
    let statuses = block_on(index.sync()).expect("composed sync");

    assert_eq!(
        statuses
            .into_iter()
            .map(|status| status.network)
            .collect::<Vec<_>>(),
        vec![first_network.clone(), second_network.clone(), idle_network]
    );
    assert_eq!(
        first.calls(),
        vec![
            Call::Watch(Box::new(Watch {
                id: "first".to_owned(),
                address: first_owner,
                asset: indexing::AssetId::native(first_network.chain),
                start: BlockPosition(2),
            })),
            Call::Sync,
        ]
    );
    assert_eq!(
        second.calls(),
        vec![
            Call::Watch(Box::new(Watch {
                id: "second".to_owned(),
                address: second_owner,
                asset: indexing::AssetId::native(second_network.chain),
                start: BlockPosition(5),
            })),
            Call::Sync,
        ]
    );
    assert_eq!(idle.calls(), vec![Call::Sync]);
}

/// A failing unit does not cost the units after it their pass.
///
/// Per-chain failure isolation is `docs/design/019`'s whole subject, and it landed
/// for the supervisor, which drives one unit per task. Bounded `sync` — what an
/// embedder without the `supervisor` feature drives — had lost it to a `?` inside
/// the loop: one chain's node refusing a call skipped every chain after it, for
/// as long as it kept refusing.
#[test]
fn a_failing_unit_does_not_stop_the_units_after_it() {
    /// A unit whose pass always fails, and counts how often it was asked.
    struct Refusing {
        network: Network,
        calls: Mutex<Vec<Call>>,
        retryable: bool,
    }

    impl Indexing for Refusing {
        fn network(&self) -> &Network {
            &self.network
        }

        fn listen(&self, _listener: Arc<dyn indexing::Listener>) {}

        fn sync<'a>(&'a self) -> BoxFuture<'a, Result<SyncStatus, IndexError>> {
            self.calls.lock().expect("calls").push(Call::Sync);
            let retryable = self.retryable;
            Box::pin(async move {
                Err(IndexError::new(
                    indexing::IndexErrorKind::Source,
                    "this node is refusing",
                    retryable,
                ))
            })
        }
    }

    impl Checkpoint for Refusing {
        fn checkpoint<'a>(
            &'a self,
            _network: &'a Network,
        ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
            Box::pin(async { Ok(None) })
        }
    }

    impl Watching for Refusing {
        fn watch<'a>(
            &'a self,
            _id: &'a str,
            _subject: &'a dyn Holder,
            _start: Start,
        ) -> BoxFuture<'a, Result<Watch, IndexError>> {
            unreachable!("this scenario never watches")
        }

        fn unwatch<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<Option<Watch>, IndexError>> {
            Box::pin(async { Ok(None) })
        }

        fn watched<'a>(
            &'a self,
            _network: &'a Network,
        ) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    impl indexing::Backfilling for Refusing {
        fn cancel<'a>(
            &'a self,
            _network: &'a Network,
        ) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    let refusing = |chain: &str, retryable: bool| {
        Arc::new(Refusing {
            network: network(chain),
            calls: Mutex::new(Vec::new()),
            retryable,
        })
    };
    // Retryable first, terminal second, so the answer cannot be "the first
    // failure" by accident.
    let first = refusing("first-chain", true);
    let second = refusing("second-chain", false);
    let last = Arc::new(Probe::new(network("last-chain"), 9));
    let index = Index::of(vec![
        first.clone() as Arc<dyn Indexing>,
        second.clone() as Arc<dyn Indexing>,
        last.clone() as Arc<dyn Indexing>,
    ])
    .expect("disjoint networks compose");

    let error = block_on(index.sync()).expect_err("two units refused");

    assert!(
        !error.retryable,
        "the terminal failure is the one a driver is told about: {}",
        error.message
    );
    assert_eq!(
        first.calls.lock().expect("calls").len(),
        1,
        "the first unit was asked"
    );
    assert_eq!(
        last.calls(),
        vec![Call::Sync],
        "and so was the unit after both failures, which used to be skipped"
    );
}

#[test]
fn rejects_a_child_whose_status_is_not_about_its_own_network() {
    struct Reporting {
        network: Network,
        status: Mutex<Option<SyncStatus>>,
    }

    impl Checkpoint for Reporting {
        fn checkpoint<'a>(
            &'a self,
            _network: &'a Network,
        ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
            Box::pin(async { Ok(None) })
        }
    }

    impl History for Reporting {
        fn history<'a>(
            &'a self,
            _of: &'a dyn Holder,
            _page: Page,
        ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
            Box::pin(async {
                Ok(TransactionPage {
                    checkpoint: None,
                    transactions: Vec::new(),
                    pending: Vec::new(),
                    next: None,
                    completeness: indexing::Completeness::Incomplete,
                })
            })
        }
    }

    impl Balances for Reporting {
        fn balance<'a>(
            &'a self,
            _of: &'a dyn Holder,
        ) -> BoxFuture<'a, Result<Balance, IndexError>> {
            Box::pin(async {
                Ok(Balance {
                    amount: indexing::Decimal::zero(),
                    observed_at: None,
                    pending: PendingValue::none(),
                    completeness: Completeness::Incomplete,
                })
            })
        }
    }

    impl Indexing for Reporting {
        fn network(&self) -> &Network {
            &self.network
        }

        fn listen(&self, _listener: Arc<dyn indexing::Listener>) {}

        fn sync<'a>(&'a self) -> BoxFuture<'a, Result<SyncStatus, IndexError>> {
            let status = self.status.lock().expect("reported status").take();
            Box::pin(async move { Ok(status.expect("one pass per fixture")) })
        }
    }

    impl Watching for Reporting {
        fn watch<'a>(
            &'a self,
            _id: &'a str,
            _subject: &'a dyn Holder,
            _start: Start,
        ) -> BoxFuture<'a, Result<Watch, IndexError>> {
            Box::pin(async { unreachable!("this fixture only reports statuses") })
        }

        fn unwatch<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<Option<Watch>, IndexError>> {
            Box::pin(async { unreachable!("this fixture only reports statuses") })
        }

        fn watched<'a>(
            &'a self,
            _network: &'a Network,
        ) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    impl indexing::Backfilling for Reporting {
        fn cancel<'a>(
            &'a self,
            _network: &'a Network,
        ) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
            Box::pin(async { unreachable!("this fixture only reports statuses") })
        }
    }

    let owned = network("owned-chain");
    let foreign = network("foreign-chain");
    let ready = SyncStatus {
        network: owned.clone(),
        checkpoint: Some(block(3)),
        pending_tip: None,
        observed_tip: Some(block(3)),
        phase: SyncPhase::Ready,
        unknown_spends: Vec::new(),
        backfill: None,
    };
    for status in [
        SyncStatus {
            network: foreign,
            ..ready.clone()
        },
        SyncStatus {
            checkpoint: None,
            ..ready.clone()
        },
    ] {
        let index = Index::of(vec![Arc::new(Reporting {
            network: owned.clone(),
            status: Mutex::new(Some(status)),
        }) as Arc<dyn Indexing>])
        .expect("one network composes");

        let error = block_on(index.sync()).expect_err("inconsistent status report");
        assert_eq!(
            error,
            IndexError::new(
                IndexErrorKind::Store,
                "indexer returned incomplete or inconsistent synchronization status",
                false,
            )
        );
    }

    let index = Index::of(vec![Arc::new(Reporting {
        network: owned,
        status: Mutex::new(Some(ready)),
    }) as Arc<dyn Indexing>])
    .expect("one network composes");
    assert_eq!(
        block_on(index.sync())
            .expect("a complete report passes")
            .len(),
        1
    );
}

/// A child that folds a balance does not make its composition fold one.
///
/// The same statement the history test below makes, now that a balance is the
/// other query a consumer holds per network rather than reaches for through the
/// thing doing the indexing. `Index` routes a balance nowhere: the fold is the
/// repository's own answer completed by one index's overlay, so a consumer
/// serving several networks collects one fold per network
/// (`wallets::Composition::balance`) and asks the one that owns the subject.
///
/// Stated as a value of the capability, so the assertion is that `Probe` — which
/// *does* implement [`Balances`] — composes into an `Index` that answers no
/// balance. A restored `impl Balances for Index` would make the `Arc<dyn
/// Balances>` below reachable from the composition and this test would stop
/// saying anything; that it is built from the child is the point.
///
/// It also removes the bound the composition used to carry. While `Index`
/// implemented its reads for a child trait that included [`Balances`], a
/// composition demanded *every* query capability of every child before it would
/// route any one of them; `Index<dyn Indexing>` asks for none.
#[test]
fn a_composition_does_not_answer_a_balance() {
    let configured = network("unrouted-balance");
    let probe = Arc::new(Probe::new(configured.clone(), 7));
    let index =
        Index::of(vec![Arc::clone(&probe) as Arc<dyn Indexing>]).expect("one network composes");
    let owner = subject(&address(&configured, "owner"));

    // The consumer asks the fold it holds for this network, never the index.
    let fold: Arc<dyn Balances> = probe.clone();
    let balance = block_on(fold.balance(&owner)).expect("the fold answers");

    assert_eq!(balance.observed_at, Some(block(7)));
    assert_eq!(index.networks(), std::slice::from_ref(&configured));
    assert_eq!(
        probe.calls(),
        vec![Call::Balance(Box::new(owner))],
        "the balance came from the unit that owns the network, not from a route"
    );
}

/// A child that answers history does not make its composition answer one: the
/// capability stops at the unit, because no indexer trait carries it and
/// `Index` no longer forwards it.
///
/// Stated as a value of the one trait `Index` has left, so the assertion is that
/// `Probe` — which *does* implement [`History`] — composes into an `Index` whose
/// only read is a balance. A restored `impl History for Index` would make the
/// `Arc<dyn History>` below reachable from the composition and this test would
/// stop saying anything; that it is built from the child is the point.
#[test]
fn a_composition_does_not_answer_a_history_page() {
    let configured = network("unrouted-history");
    let probe = Arc::new(Probe::new(configured.clone(), 7));
    let index =
        Index::of(vec![Arc::clone(&probe) as Arc<dyn Indexing>]).expect("one network composes");
    let owner = subject(&address(&configured, "owner"));

    // The consumer asks the reader it holds for this network, never the index.
    let reader: Arc<dyn History> = probe.clone();
    let page = block_on(reader.history(&owner, Page::first(25))).expect("the reader answers");

    assert_eq!(page.checkpoint, Some(block(7)));
    assert_eq!(index.networks(), std::slice::from_ref(&configured));
    assert_eq!(
        probe.calls(),
        vec![Call::History(Box::new(owner), Page::first(25))],
        "the page came from the unit that owns the network, not from a route"
    );
}
