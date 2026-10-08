//! Contract coverage for `Indexers`: assembly over one store, network routing of
//! checkpoints and watches, and the status report it accepts from each child.
//!
//! No query is routed, and none can be: a balance, a history page and a
//! spendable set are a repository's reads, and the capabilities a consumer
//! declares for their pending-inclusive forms live with the consumer
//! (`indexing::History`, `indexing::Balances`), where this crate cannot name
//! them.

use std::sync::{Arc, Mutex};

use futures_executor::block_on;
use indexing::{
    Address, Birthday, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, BoxFuture,
    ChainName, Checkpoint, Holder, IndexError, IndexErrorKind, Indexers, Indexing, Network,
    Outcome, Phase, Subject, Watch, Watching,
};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Checkpoint(Network),
    Watch(Box<Watch>),
    Unwatch(String),
    Watched(Network),
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

impl Indexing for Probe {
    fn network(&self) -> &Network {
        &self.network
    }

    fn listen(&self, _listener: Arc<dyn indexing::Listener>) {}

    fn schedule(&self) -> indexing::Schedule {
        indexing::Schedule::every(std::time::Duration::from_millis(1))
    }

    fn sync<'a>(&'a self) -> BoxFuture<'a, Result<Outcome, IndexError>> {
        self.record(Call::Sync);
        let status = Outcome {
            network: self.network.clone(),
            checkpoint: Some(self.checkpoint.clone()),
            pending_tip: None,
            observed_tip: Some(self.checkpoint.clone()),
            phase: Phase::Ready,
            unmatched_spends: Vec::new(),
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
        start: Birthday,
    ) -> BoxFuture<'a, Result<Watch, IndexError>> {
        let watch = Watch {
            id: id.to_owned(),
            address: subject.address().clone(),
            asset: subject.asset().clone(),
            start: match start {
                Birthday::At(position) => position,
                Birthday::Fresh => self.checkpoint.position,
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
/// `Outcome`, an `Indexers` answers `networks()` and a vector of them, and the
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
    let watched = block_on(indexer.watch(
        "owner",
        &subject(&owner),
        Birthday::At(BlockPosition(height)),
    ))
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
    let index = Indexers::of(vec![
        Arc::new(Probe::new(composed_network.clone(), 7)) as Arc<dyn Indexing>
    ])
    .expect("one chain composes");
    exercise_reads(&index, &composed_network, 7);
    assert_eq!(index.networks(), std::slice::from_ref(&composed_network));
    assert_eq!(
        index
            .units()
            .iter()
            .map(|unit| block_on(unit.sync()).expect("one unit, one status").network)
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
    let index = Indexers::of(vec![
        first.clone() as Arc<dyn Indexing>,
        second.clone() as Arc<dyn Indexing>,
    ])
    .expect("disjoint networks compose");

    let checkpoint = block_on(index.checkpoint(&first_network)).expect("checkpoint");
    assert_eq!(checkpoint, Some(block(4)));

    let owner = address(&second_network, "owner");
    let watched = block_on(index.watch("owner", &subject(&owner), Birthday::Fresh)).expect("watch");
    assert_eq!(watched.address, owner);

    assert_eq!(first.calls(), vec![Call::Checkpoint(first_network)]);
    assert_eq!(second.calls(), vec![Call::Watch(Box::new(watched))]);
}

#[test]
fn an_empty_index_allows_configuration_but_does_not_report_readiness() {
    let index = Indexers::<dyn Indexing>::of(Vec::new()).expect("empty collection");
    assert!(index.units().is_empty());
    assert!(index.networks().is_empty());
    assert!(!indexing::SyncHealth::starting(index.networks()).ready());
}

#[test]
fn rejects_duplicate_networks() {
    let shared_network = network("same-chain");
    let result = Indexers::of(vec![
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
    let index = Indexers::of(vec![
        Arc::new(Probe::new(configured, 1)) as Arc<dyn Indexing>
    ])
    .expect("one network composes");

    let checkpoint_error = block_on(index.checkpoint(&missing)).expect_err("missing checkpoint");
    let watch_error = block_on(index.watch(
        "owner",
        &subject(&address(&missing, "owner")),
        Birthday::Fresh,
    ))
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
    let index = Indexers::of(vec![
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
        block_on(index.watch(id, &subject(owner), Birthday::At(start))).expect("watch");
    }
    let statuses: Vec<_> = index
        .units()
        .iter()
        .map(|unit| block_on(unit.sync()).expect("unit sync"))
        .collect();

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

#[test]
fn rejects_a_child_whose_status_is_not_about_its_own_network() {
    struct Reporting {
        network: Network,
        status: Mutex<Option<Outcome>>,
    }

    impl Checkpoint for Reporting {
        fn checkpoint<'a>(
            &'a self,
            _network: &'a Network,
        ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
            Box::pin(async { Ok(None) })
        }
    }

    impl Indexing for Reporting {
        fn network(&self) -> &Network {
            &self.network
        }

        fn listen(&self, _listener: Arc<dyn indexing::Listener>) {}

        fn schedule(&self) -> indexing::Schedule {
            indexing::Schedule::every(std::time::Duration::from_millis(1))
        }

        fn sync<'a>(&'a self) -> BoxFuture<'a, Result<Outcome, IndexError>> {
            let status = self.status.lock().expect("reported status").take();
            Box::pin(async move { Ok(status.expect("one pass per fixture")) })
        }
    }

    impl Watching for Reporting {
        fn watch<'a>(
            &'a self,
            _id: &'a str,
            _subject: &'a dyn Holder,
            _start: Birthday,
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

    let owned = network("owned-chain");
    let foreign = network("foreign-chain");
    let ready = Outcome {
        network: owned.clone(),
        checkpoint: Some(block(3)),
        pending_tip: None,
        observed_tip: Some(block(3)),
        phase: Phase::Ready,
        unmatched_spends: Vec::new(),
        backfill: None,
    };
    for status in [
        Outcome {
            network: foreign,
            ..ready.clone()
        },
        Outcome {
            checkpoint: None,
            ..ready.clone()
        },
    ] {
        let index = Indexers::of(vec![Arc::new(Reporting {
            network: owned.clone(),
            status: Mutex::new(Some(status)),
        }) as Arc<dyn Indexing>])
        .expect("one network composes");

        let error = block_on(index.units()[0].sync()).expect_err("inconsistent status report");
        assert_eq!(
            error,
            IndexError::new(
                IndexErrorKind::Store,
                "indexer returned incomplete or inconsistent synchronization status",
                false,
            )
        );
    }

    let index = Indexers::of(vec![Arc::new(Reporting {
        network: owned,
        status: Mutex::new(Some(ready)),
    }) as Arc<dyn Indexing>])
    .expect("one network composes");
    block_on(index.units()[0].sync()).expect("a complete report passes");
}
