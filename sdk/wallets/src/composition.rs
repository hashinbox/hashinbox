//! How an application turns configured chains into one running system.
//!
//! Every chain assembles itself the same way — a `Config` value connects to a
//! node and verifies its identity, and the verified node composes over an
//! opened repository — so what each one hands back is one type, declared here
//! once rather than three times: [`ComposedChain`]. [`Connection`] erases the
//! chain that produced it, and [`Composition`] folds any number of them into
//! what a process holds: one index, one handle per query an edge reads — a
//! history page and a balance — one factory map, one unit per configured asset,
//! one cadence per network.
//!
//! The two phases never merge. **Every identity check finishes before any
//! storage is opened**, so a deployment that names the wrong node fails
//! without having written a byte.
//! [`Composition::build`] takes chains that are already verified — the data
//! flow states the order — and [`Composition::verify_then_build`] states it
//! for a caller that would otherwise have to remember it.

use std::{future::Future, sync::Arc, time::Duration};

use base::{BoxFuture, Units};
use indexing::{Balances, History, Index, Indexing, Router, Schedule, Schedules};

use crate::{SubmissionRegistrar, WalletError, WalletErrorKind, WalletFactory, Wallets};

/// One chain network's composed parts, as an application registers them.
///
/// Every chain returns this: Bitcoin with one factory, Ethereum and Solana
/// with one per allowlisted token, all over the one repository of their own
/// network.
pub struct ComposedChain<A> {
    /// The index of this one network, already mirroring its watch list to the
    /// store it was composed over. One chain's `ChainHandle::compose` returns one
    /// network's indexer, which is what [`Indexing`] is: [`Composition`] is the
    /// only value here that holds more than one.
    pub index: Arc<dyn Indexing>,
    /// This network's history reader: the repository's own confirmed pages
    /// joined to the blocks `index` still holds above its confirmed line
    /// ([`indexing::PendingJoin`]).
    ///
    /// A field rather than a capability of `index`, because it is neither
    /// half's alone: the page is the repository's query and the pending blocks
    /// are the index's, and a chain is the place that holds both, so it is the
    /// place that pairs them. [`Composition`] collects them into the one handle
    /// a consumer serving several networks holds.
    pub history: Arc<dyn History>,
    /// This network's balance fold: the repository's own total for a holding,
    /// completed by the blocks `index` still holds above its confirmed line
    /// ([`indexing::PendingJoin`]).
    ///
    /// Stated beside `history` and for the same reason. A balance is a query
    /// over stored rows, and this chain is the one place that holds both the
    /// rows' owner and the index whose pending blocks finish them, so it is the
    /// one place that can name what answers it. [`Composition`] collects them
    /// into the one handle a consumer serving several networks holds — a
    /// consumer never reaches a query through an index.
    pub balance: Arc<dyn Balances>,
    /// The asset tags and factories of this chain, all in one network.
    pub factories: Vec<(A, Box<dyn WalletFactory>)>,
    /// The unit of every asset this chain declares — its own currency, and
    /// each allowlisted token in the scale its contract was checked against.
    /// A chain's configuration is the authority; no wallet carries it.
    pub units: Units,
    /// How often this chain alone polls its node. Each chain is supervised on
    /// its own task, so a fast chain does not set everyone's cadence.
    pub poll: Duration,
}

/// A chain whose node identity has been verified; it composes itself over the
/// repository once storage is open.
///
/// Three chains produce three different verified-node types and one vector has
/// to hold them together, so something must erase them; a trait rather than a
/// boxed closure, so each chain crate can state the store capabilities its own
/// composition needs. An application writes none of it.
///
/// It no longer answers which network it indexes. It had to while a repository
/// was opened per network and the network was what selected one; one store
/// serves every network now, so selecting a repository no longer needs it.
pub trait Connection<A, R>: Send {
    /// This chain's parts, built over `repository`. Background send work a
    /// chain cannot finish inside its own call goes to `registrar`.
    fn compose<'a>(
        self: Box<Self>,
        repository: R,
        registrar: &'a Arc<dyn SubmissionRegistrar>,
    ) -> BoxFuture<'a, Result<ComposedChain<A>, WalletError>>;
}

/// Everything a process runs on, built once: every chain's index behind one
/// [`Index`], every query an edge reads behind one handle of its own, every
/// factory under its asset tag, every declared asset's unit under its identity,
/// and each network's polling cadence for `indexing::Supervisor`.
///
/// Nothing above this value names a chain: an asset tag selects a factory and
/// a network selects an index — or the one handle that answers a query for that
/// network, which a consumer holds as a value rather than reaching for through
/// the index.
pub struct Composition<A: Ord> {
    /// Every configured chain's index, as one.
    pub index: Arc<Index>,
    /// Every configured network's history reader, as one: the narrow capability
    /// a history consumer declares, answered by the reader that owns the
    /// holding's network.
    ///
    /// Router here rather than by `indexing::Index`, because a page is a
    /// repository's own query and not something to reach through an indexer.
    pub history: Arc<dyn History>,
    /// Every configured network's balance fold, as one: the same narrow
    /// capability, answered by the fold that owns the holding's network.
    ///
    /// Router here for the same reason a page is: a balance is a query over a
    /// repository's rows, and a consumer asks the owner of the network directly
    /// rather than through the composed index it happens to be driven by.
    pub balance: Arc<dyn Balances>,
    /// Every configured chain's wallet factories, keyed by asset tag.
    pub wallets: Wallets<A>,
    /// Every configured asset's unit, by identity: what an edge reporting an
    /// indexed amount scales it by.
    pub units: Units,
    /// What each network polls at, for the supervisor that drives `index`.
    pub schedules: Schedules,
}

impl<A> Composition<A>
where
    A: Clone + Ord + Send + Sync + 'static,
{
    /// Cadence of a network no composed chain claimed. Every chain states its
    /// own, so it stands only for a `Index` extended by other means.
    const POLL: Duration = Duration::from_secs(1);

    /// Composes every verified chain over the repository `repository` gives
    /// its network, and folds the results together.
    ///
    /// `chains` cannot exist before every identity check has returned, which
    /// is the whole of the startup ordering: a caller holding this vector has
    /// already passed the phase where opening storage would have been early.
    ///
    /// # Errors
    ///
    /// Whatever `repository` reports for a network, whatever a chain reports
    /// while building its index and factories, a duplicate asset tag across
    /// two chains, [`WalletErrorKind::Unsupported`] for a factory whose chain
    /// declared no unit for the asset it makes wallets of, and
    /// [`indexing::IndexErrorKind::Conflict`] when two chains claim one
    /// network.
    pub async fn build<R, E>(
        chains: Vec<Box<dyn Connection<A, R>>>,
        repository: R,
        registrar: &Arc<dyn SubmissionRegistrar>,
    ) -> Result<Self, E>
    where
        R: Clone,
        E: From<WalletError>,
    {
        let mut indexes = Vec::with_capacity(chains.len());
        let mut readers = Vec::with_capacity(chains.len());
        let mut folds = Vec::with_capacity(chains.len());
        let mut schedules = Schedules::every(Self::POLL);
        let mut wallets = Wallets::new();
        let mut units = Units::default();
        for chain in chains {
            let ComposedChain {
                index,
                history,
                balance,
                factories,
                poll,
                units: declared,
            } = chain
                .compose(repository.clone(), registrar)
                .await
                .map_err(E::from)?;
            schedules = schedules.with(index.network(), Schedule::every(poll));
            Self::register(&mut wallets, factories, &declared)?;
            units.extend(declared);
            readers.push((index.network().clone(), history));
            folds.push((index.network().clone(), balance));
            indexes.push(index);
        }
        let failed = |error| E::from(WalletError::from(error));
        let index = Index::of(indexes).map_err(failed)?;
        Ok(Self {
            index: Arc::new(index),
            history: Arc::new(Router::new(readers).map_err(failed)?),
            balance: Arc::new(Router::new(folds).map_err(failed)?),
            wallets,
            units,
            schedules,
        })
    }

    /// Adds one chain's factories to the map, each under its own tag.
    ///
    /// Every configured asset must be reportable in the units a caller reads it
    /// in, so a factory whose own chain declared no unit for its asset never
    /// reaches the map.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unsupported`] for such a factory, and a duplicate tag.
    fn register<E: From<WalletError>>(
        wallets: &mut Wallets<A>,
        factories: Vec<(A, Box<dyn WalletFactory>)>,
        declared: &Units,
    ) -> Result<(), E> {
        for (tag, factory) in factories {
            if declared.of(factory.network(), &factory.asset()).is_none() {
                return Err(E::from(WalletError::new(
                    WalletErrorKind::Unsupported,
                    "a chain registered a wallet factory for an asset whose unit it \
                     did not declare",
                )));
            }
            wallets.add(tag, factory).map_err(E::from)?;
        }
        Ok(())
    }

    /// Awaits `verification`, opens storage only then, and builds over it.
    ///
    /// `open` is not called at all if any identity check fails, or if the
    /// caller drops this future while they are still running. That is the one
    /// guarantee this function adds over [`Self::build`], and it is why a
    /// deployment pointed at the wrong node leaves no database behind.
    ///
    /// # Errors
    ///
    /// The first identity failure, the storage failure that follows a
    /// successful verification, and [`Self::build`]'s own refusals.
    pub async fn verify_then_build<R, S, E, V, O, F>(
        verification: V,
        open: O,
        repository: impl FnOnce(&S) -> Result<R, E>,
        registrar: &Arc<dyn SubmissionRegistrar>,
    ) -> Result<Self, E>
    where
        R: Clone,
        E: From<WalletError>,
        V: Future<Output = Result<Vec<Box<dyn Connection<A, R>>>, E>>,
        O: FnOnce() -> F,
        F: Future<Output = Result<S, E>>,
    {
        let chains = verification.await?;
        let store = open().await?;
        Self::build(chains, repository(&store)?, registrar).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use base::{AssetId, ChainName, Unit};
    use indexing::{
        Balance, Birthday, BlockRef, Checkpoint, Holder, IndexError, IndexErrorKind, Indexing,
        Listener, Network, Page, Subject, SyncStatus, TransactionPage, Watch, Watching, memory,
    };

    use super::*;
    use crate::{
        RegistrationFuture, SubmissionTask, WalletErrorKind, wallets::tests::FixtureFactory,
    };

    fn network(chain: &str) -> Network {
        Network::new(ChainName::new(chain), "net")
    }

    /// An index of one network that indexes nothing: composition reads its
    /// network and holds it, and nothing here drives it.
    ///
    /// **It folds no balance**, and that is now expressible: while
    /// `ComposedChain::index` was a wider indexer trait carrying `Balances`,
    /// this fixture had to implement one it never answered — a courier, and the
    /// only proof that nothing read it was an `unreachable!`. A balance reaches
    /// [`FixtureFold`] and the type system says so.
    struct FixtureIndex(Network);

    impl Indexing for FixtureIndex {
        fn network(&self) -> &Network {
            &self.0
        }

        fn listen(&self, _listener: Arc<dyn Listener>) {}

        fn sync<'a>(&'a self) -> BoxFuture<'a, Result<SyncStatus, IndexError>> {
            Box::pin(async { unreachable!("a fixture index never advances") })
        }
    }

    impl Checkpoint for FixtureIndex {
        fn checkpoint<'a>(
            &'a self,
            _network: &'a Network,
        ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
            Box::pin(async { Ok(None) })
        }
    }

    /// One network's history reader, answering a page that names which network
    /// it came from: what `Histories` routes to, without a repository or a
    /// pending block in sight.
    struct FixtureHistory(Network);

    impl History for FixtureHistory {
        fn history<'a>(
            &'a self,
            of: &'a dyn Holder,
            _page: Page,
        ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
            assert_eq!(
                of.address().network,
                self.0,
                "a reader is only ever asked about its own network"
            );
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

    /// One network's balance fold, answering only about its own network: what
    /// `Folds` routes to, and a separate value from the index so a read that
    /// went through the indexer instead would panic.
    struct FixtureFold(Network);

    impl Balances for FixtureFold {
        fn balance<'a>(&'a self, of: &'a dyn Holder) -> BoxFuture<'a, Result<Balance, IndexError>> {
            assert_eq!(
                of.address().network,
                self.0,
                "a fold is only ever asked about its own network"
            );
            Box::pin(async {
                Ok(Balance {
                    amount: indexing::Decimal::zero(),
                    observed_at: None,
                    pending: indexing::PendingValue::none(),
                    completeness: indexing::Completeness::Incomplete,
                    opening: None,
                })
            })
        }
    }

    impl Watching for FixtureIndex {
        fn watch<'a>(
            &'a self,
            _id: &'a str,
            _subject: &'a dyn Holder,
            _start: Birthday,
        ) -> BoxFuture<'a, Result<Watch, IndexError>> {
            unreachable!("a fixture index watches nothing")
        }

        fn unwatch<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<Option<Watch>, IndexError>> {
            unreachable!("a fixture index watches nothing")
        }

        fn watched<'a>(
            &'a self,
            _network: &'a Network,
        ) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    impl indexing::Backfilling for FixtureIndex {
        fn cancel_backfill<'a>(
            &'a self,
            _network: &'a Network,
        ) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
            unreachable!("a fixture index watches nothing")
        }
    }

    /// One chain, in the position a chain crate's verified `ChainHandle` takes: it
    /// names its network before storage is open, and composes over the
    /// repository it is handed.
    struct FixtureChain {
        chain: &'static str,
        tag: &'static str,
        /// Whether it declares the unit of the asset its factory makes wallets
        /// of, as every real chain's configuration does.
        declares: bool,
    }

    impl FixtureChain {
        const POLL: Duration = Duration::from_millis(250);

        /// One chain whose single factory is tagged after the chain itself.
        fn of(chain: &'static str) -> Box<dyn Connection<&'static str, Handle>> {
            Box::new(Self {
                chain,
                tag: chain,
                declares: true,
            })
        }

        /// A chain that registers a factory without declaring that asset's
        /// unit, which no configuration can produce and composition refuses.
        fn undeclared(chain: &'static str) -> Box<dyn Connection<&'static str, Handle>> {
            Box::new(Self {
                chain,
                tag: chain,
                declares: false,
            })
        }

        /// A second chain whose factory claims `tag`, which another chain may
        /// already have taken.
        fn tagged(
            chain: &'static str,
            tag: &'static str,
        ) -> Box<dyn Connection<&'static str, Handle>> {
            Box::new(Self {
                chain,
                tag,
                declares: true,
            })
        }
    }

    impl Connection<&'static str, Handle> for FixtureChain {
        fn compose<'a>(
            self: Box<Self>,
            _repository: Handle,
            _registrar: &'a Arc<dyn SubmissionRegistrar>,
        ) -> BoxFuture<'a, Result<ComposedChain<&'static str>, WalletError>> {
            Box::pin(async move {
                let factory = FixtureFactory::of(network(self.chain));
                let declared = AssetId::native(ChainName::new(self.chain));
                let units = if self.declares {
                    Units::from_iter([((network(self.chain), declared), Unit::token(0))])
                } else {
                    Units::default()
                };
                Ok(ComposedChain {
                    index: Arc::new(FixtureIndex(network(self.chain))),
                    history: Arc::new(FixtureHistory(network(self.chain))),
                    balance: Arc::new(FixtureFold(network(self.chain))),
                    factories: vec![(self.tag, Box::new(factory) as Box<dyn WalletFactory>)],
                    units,
                    poll: Self::POLL,
                })
            })
        }
    }

    /// Registers nothing: a fixture chain finishes its sends inside its call.
    struct FixtureRegistrar;

    impl SubmissionRegistrar for FixtureRegistrar {
        fn register<'a>(&'a self, _task: SubmissionTask) -> RegistrationFuture<'a> {
            unreachable!("a fixture chain registers no submission")
        }
    }

    fn registrar() -> Arc<dyn SubmissionRegistrar> {
        Arc::new(FixtureRegistrar)
    }

    /// The one store an application opens, recording how many chains composed
    /// over it.
    #[derive(Default)]
    struct Stores(memory::Repository, Arc<Mutex<Vec<String>>>);

    /// A store handle each chain gets a clone of, which records that it did.
    struct Handle(memory::Repository, Arc<Mutex<Vec<String>>>);

    impl Stores {
        fn handle(&self) -> Handle {
            Handle(self.0.clone(), Arc::clone(&self.1))
        }

        fn shared(&self) -> usize {
            self.1.lock().expect("clones").len()
        }
    }

    impl Clone for Handle {
        fn clone(&self) -> Self {
            self.1.lock().expect("clones").push(String::from("clone"));
            Self(self.0.clone(), Arc::clone(&self.1))
        }
    }

    #[tokio::test]
    async fn two_chains_become_one_index_one_map_and_one_cadence_each() {
        let stores = Stores::default();

        let composed = Composition::<&'static str>::build::<Handle, WalletError>(
            vec![FixtureChain::of("alpha"), FixtureChain::of("beta")],
            stores.handle(),
            &registrar(),
        )
        .await
        .expect("two disjoint chains");

        assert_eq!(
            stores.shared(),
            2,
            "one store reached both chains, by one clone of the handle each — \
             never one repository opened per network",
        );
        assert_eq!(
            composed.index.networks(),
            [network("alpha"), network("beta")]
        );
        assert_eq!(
            composed
                .wallets
                .assets()
                .iter()
                .map(|asset| asset.tag)
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        assert_eq!(
            composed.schedules.of(&network("alpha")).interval,
            FixtureChain::POLL,
            "each chain keeps the cadence it composed with"
        );
        for chain in ["alpha", "beta"] {
            assert_eq!(
                composed
                    .units
                    .of(&network(chain), &AssetId::native(ChainName::new(chain))),
                Some(&Unit::token(0)),
                "every chain's declared units are folded into one map"
            );
        }
    }

    /// Every chain's own history reader behind one handle, asked by network.
    ///
    /// The capability a consumer declares is one `Arc<dyn History>`, and this is
    /// what satisfies it: each chain built a reader over *its* repository and
    /// *its* pending blocks, and the route is the holding's own network. Nothing here
    /// reaches a page through an indexer — `Index` has no history to route — and
    /// a network no chain claimed is the same refusal every other read gives.
    #[tokio::test]
    async fn one_history_reader_per_network_answers_the_one_that_owns_the_holding() {
        let stores = Stores::default();

        let composed = Composition::<&'static str>::build::<Handle, WalletError>(
            vec![FixtureChain::of("alpha"), FixtureChain::of("beta")],
            stores.handle(),
            &registrar(),
        )
        .await
        .expect("two disjoint chains");

        for chain in ["alpha", "beta"] {
            let of = Subject {
                address: base::Address::new(network(chain), "owner"),
                asset: AssetId::native(ChainName::new(chain)),
            };
            composed
                .history
                .history(&of, Page::first(10))
                .await
                .expect("the reader that owns this network answers");
        }

        let elsewhere = Subject {
            address: base::Address::new(network("gamma"), "owner"),
            asset: AssetId::native(ChainName::new("gamma")),
        };
        let refused = composed
            .history
            .history(&elsewhere, Page::first(10))
            .await
            .expect_err("no chain claimed this network");
        assert_eq!(refused.kind, IndexErrorKind::NetworkMismatch);
        assert!(!refused.retryable);
    }

    /// Every chain's own balance fold behind one handle, asked by network.
    ///
    /// The same shape a page takes, and for the same reason: a total is stored
    /// rows completed by one network's pending blocks, so the consumer holds one fold
    /// per network and asks the owner of the holding. `FixtureIndex::balance`
    /// panics, which is what proves no read of this handle reached an indexer.
    #[tokio::test]
    async fn one_balance_fold_per_network_answers_the_one_that_owns_the_holding() {
        let stores = Stores::default();

        let composed = Composition::<&'static str>::build::<Handle, WalletError>(
            vec![FixtureChain::of("alpha"), FixtureChain::of("beta")],
            stores.handle(),
            &registrar(),
        )
        .await
        .expect("two disjoint chains");

        for chain in ["alpha", "beta"] {
            let of = Subject {
                address: base::Address::new(network(chain), "owner"),
                asset: AssetId::native(ChainName::new(chain)),
            };
            composed
                .balance
                .balance(&of)
                .await
                .expect("the fold that owns this network answers");
        }

        let elsewhere = Subject {
            address: base::Address::new(network("gamma"), "owner"),
            asset: AssetId::native(ChainName::new("gamma")),
        };
        let refused = composed
            .balance
            .balance(&elsewhere)
            .await
            .expect_err("no chain claimed this network");
        assert_eq!(refused.kind, IndexErrorKind::NetworkMismatch);
        assert!(!refused.retryable);
    }

    /// The two ways two chains can contradict each other, and the one way a
    /// chain can contradict itself: all refused before anything runs.
    #[tokio::test]
    async fn one_network_twice_one_tag_twice_and_an_unpriced_asset_are_refused() {
        let stores = Stores::default();
        let registrar = registrar();
        let build = async |chains| {
            Composition::<&'static str>::build::<Handle, WalletError>(
                chains,
                stores.handle(),
                &registrar,
            )
            .await
            .err()
            .expect("a contradiction")
            .kind
        };

        let tag = build(vec![
            FixtureChain::of("alpha"),
            FixtureChain::tagged("beta", "alpha"),
        ])
        .await;
        let net = build(vec![
            FixtureChain::of("alpha"),
            FixtureChain::tagged("alpha", "beta"),
        ])
        .await;

        let unpriced = build(vec![FixtureChain::undeclared("alpha")]).await;

        assert_eq!(tag, WalletErrorKind::Duplicate);
        assert_eq!(net, WalletErrorKind::Conflict);
        assert_eq!(unpriced, WalletErrorKind::Unsupported);
    }

    #[tokio::test]
    async fn every_identity_failure_observes_zero_storage_calls() {
        for failure in [
            "wrong Bitcoin identity",
            "wrong Ethereum identity",
            "wrong Solana genesis",
            "missing Memo",
            "non-executable Memo",
            "malformed Memo",
            "Memo below finalized floor",
        ] {
            let calls = AtomicUsize::new(0);

            let refused = Composition::<&'static str>::verify_then_build(
                async { Err(WalletError::new(WalletErrorKind::Unsupported, failure)) },
                || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    async { Ok(Stores::default()) }
                },
                |store: &Stores| -> Result<Handle, WalletError> { Ok(store.handle()) },
                &registrar(),
            )
            .await;

            assert_eq!(refused.err().expect("refused").message, failure);
            assert_eq!(calls.load(Ordering::Relaxed), 0, "failure: {failure}");
        }
    }

    #[test]
    fn pending_and_cancelled_verification_never_opens_storage() {
        let calls = AtomicUsize::new(0);
        let registrar = registrar();
        let mut startup = Box::pin(Composition::<&'static str>::verify_then_build(
            std::future::pending(),
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                async { Ok(Stores::default()) }
            },
            |store: &Stores| -> Result<Handle, WalletError> { Ok(store.handle()) },
            &registrar,
        ));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());

        assert!(startup.as_mut().poll(&mut context).is_pending());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        drop(startup);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn storage_opens_once_after_verification_and_its_failure_propagates() {
        let order = AtomicUsize::new(0);

        let refused = Composition::<&'static str>::verify_then_build(
            async {
                assert_eq!(order.fetch_add(1, Ordering::Relaxed), 0);
                Ok(vec![FixtureChain::of("alpha")])
            },
            || {
                assert_eq!(order.fetch_add(1, Ordering::Relaxed), 1);
                async {
                    Err(WalletError::new(
                        WalletErrorKind::Unavailable,
                        "storage open failed",
                    ))
                }
            },
            |store: &Stores| -> Result<Handle, WalletError> { Ok(store.handle()) },
            &registrar(),
        )
        .await;

        assert_eq!(
            refused.err().expect("refused").message,
            "storage open failed"
        );
        assert_eq!(order.load(Ordering::Relaxed), 2);
        let opened = AtomicUsize::new(0);
        let composed = Composition::verify_then_build(
            async { Ok(vec![FixtureChain::of("alpha")]) },
            || {
                opened.fetch_add(1, Ordering::Relaxed);
                async { Ok(Stores::default()) }
            },
            |store: &Stores| -> Result<Handle, WalletError> { Ok(store.handle()) },
            &registrar(),
        )
        .await
        .expect("one chain over one store");

        assert_eq!(opened.load(Ordering::Relaxed), 1);
        assert_eq!(composed.index.networks(), [network("alpha")]);
    }
}
