//! How an application turns configured chains into one running system.
//!
//! Every chain assembles itself the same way — a `Config` value connects to a
//! node and verifies its identity, and the verified node composes over an
//! opened repository — so what each one hands back is one type, declared here
//! once rather than three times: [`ComposedChain`]. [`Connection`] erases the
//! chain that produced it, and [`Composition`] folds any number of them into
//! what a process holds: one index, each network's pending blocks, one factory
//! map, one unit per configured asset, one cadence per network. Reading
//! balances and history is the application's own business.
//!
//! The two phases never merge. **Every identity check finishes before any
//! storage is opened**, so a deployment that names the wrong node fails
//! without having written a byte.
//! [`Composition::build`] takes chains that are already verified — the data
//! flow states the order — and [`Composition::verify_then_build`] states it
//! for a caller that would otherwise have to remember it.

use std::{future::Future, sync::Arc, time::Duration};

use base::{BoxFuture, Units};
use indexing::{Indexers, Indexing, Network, Pending, Schedule, Schedules};
use wallets::{SubmissionRegistrar, WalletError, WalletErrorKind, WalletFactory, Wallets};

use crate::Error;

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
    /// The blocks `index` holds above its confirmed line, for an application
    /// that completes its own stored reads with them.
    pub pending: Arc<dyn Pending>,
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
    ) -> BoxFuture<'a, Result<ComposedChain<A>, Error>>;
}

/// Everything a process runs on, built once: every chain's index behind one
/// [`Indexers`], every query an edge reads behind one handle of its own, every
/// factory under its asset tag, every declared asset's unit under its identity,
/// and each network's polling cadence for `indexing::Supervisor`.
///
/// Nothing above this value names a chain: an asset tag selects a factory and
/// a network selects an index — or the one handle that answers a query for that
/// network, which a consumer holds as a value rather than reaching for through
/// the index.
pub struct Composition<A: Ord> {
    /// Every configured chain's index, as one.
    pub index: Arc<Indexers>,
    /// Every configured network's pending blocks, in composition order.
    pub pending: Vec<(Network, Arc<dyn Pending>)>,
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
    /// own, so it stands only for a `Indexers` extended by other means.
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
    /// an [`Error`] when two chains claim one network.
    pub async fn build<R, E>(
        chains: Vec<Box<dyn Connection<A, R>>>,
        repository: R,
        registrar: &Arc<dyn SubmissionRegistrar>,
    ) -> Result<Self, E>
    where
        R: Clone,
        E: From<WalletError> + From<Error>,
    {
        let mut indexes = Vec::with_capacity(chains.len());
        let mut pending = Vec::with_capacity(chains.len());
        let mut schedules = Schedules::every(Self::POLL);
        let mut wallets = Wallets::new();
        let mut units = Units::default();
        for chain in chains {
            let ComposedChain {
                index,
                pending: held,
                factories,
                poll,
                units: declared,
            } = chain
                .compose(repository.clone(), registrar)
                .await
                .map_err(E::from)?;
            schedules = schedules.with(index.network(), Schedule::every(poll));
            Self::register::<E>(&mut wallets, factories, &declared)?;
            units.extend(declared);
            pending.push((index.network().clone(), held));
            indexes.push(index);
        }
        let failed = |error| E::from(Error::from(error));
        let index = Indexers::of(indexes).map_err(failed)?;
        Ok(Self {
            index: Arc::new(index),
            pending,
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
        E: From<WalletError> + From<Error>,
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
        Birthday, BlockRef, Checkpoint, Holder, IndexError, Indexing, Listener, Network, Outcome,
        Sample, Watch, Watching, memory,
    };

    use super::*;
    use wallets::{RegistrationFuture, SubmissionTask, testing::Factory};

    fn network(chain: &str) -> Network {
        Network::new(ChainName::new(chain), "net")
    }

    /// An index of one network that indexes nothing: composition reads its
    /// network and holds it, and nothing here drives it.
    ///
    struct FixtureIndex(Network);

    impl Indexing for FixtureIndex {
        fn network(&self) -> &Network {
            &self.0
        }

        fn listen(&self, _listener: Arc<dyn Listener>) {}

        fn sync<'a>(&'a self) -> BoxFuture<'a, Result<Outcome, IndexError>> {
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

    impl Pending for FixtureIndex {
        fn sampled(&self) -> Result<Sample, IndexError> {
            Ok(Sample::new(Vec::new(), 0))
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
        ) -> BoxFuture<'a, Result<ComposedChain<&'static str>, Error>> {
            Box::pin(async move {
                let factory = Factory::new(network(self.chain));
                let declared = AssetId::native(ChainName::new(self.chain));
                let units = if self.declares {
                    Units::from_iter([((network(self.chain), declared), Unit::token(0))])
                } else {
                    Units::default()
                };
                Ok(ComposedChain {
                    index: Arc::new(FixtureIndex(network(self.chain))),
                    pending: Arc::new(FixtureIndex(network(self.chain))),
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

        let composed = Composition::<&'static str>::build::<Handle, Error>(
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

    /// Every chain's pending blocks, once per network, in composition order.
    #[tokio::test]
    async fn pending_blocks_are_held_once_per_network() {
        let stores = Stores::default();

        let composed = Composition::<&'static str>::build::<Handle, Error>(
            vec![FixtureChain::of("alpha"), FixtureChain::of("beta")],
            stores.handle(),
            &registrar(),
        )
        .await
        .expect("two disjoint chains");

        let networks = composed
            .pending
            .iter()
            .map(|(network, _)| network.clone())
            .collect::<Vec<_>>();
        assert_eq!(networks, [network("alpha"), network("beta")]);
    }

    /// The two ways two chains can contradict each other, and the one way a
    /// chain can contradict itself: all refused before anything runs.
    #[tokio::test]
    async fn one_network_twice_one_tag_twice_and_an_unpriced_asset_are_refused() {
        let stores = Stores::default();
        let registrar = registrar();
        let build = async |chains| {
            Composition::<&'static str>::build::<Handle, Error>(chains, stores.handle(), &registrar)
                .await
                .err()
                .expect("a contradiction")
                .message
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

        assert_eq!(tag, "a wallet factory is already registered for this asset");
        assert_eq!(net, "indexers contain the same network");
        assert!(
            unpriced.contains("did not declare"),
            "unpriced asset: {unpriced}"
        );
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
                async { Err(Error::refused(failure)) },
                || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    async { Ok(Stores::default()) }
                },
                |store: &Stores| -> Result<Handle, Error> { Ok(store.handle()) },
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
            |store: &Stores| -> Result<Handle, Error> { Ok(store.handle()) },
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
                async { Err(Error::refused("storage open failed")) }
            },
            |store: &Stores| -> Result<Handle, Error> { Ok(store.handle()) },
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
            |store: &Stores| -> Result<Handle, Error> { Ok(store.handle()) },
            &registrar(),
        )
        .await
        .expect("one chain over one store");

        assert_eq!(opened.load(Ordering::Relaxed), 1);
        assert_eq!(composed.index.networks(), [network("alpha")]);
    }
}
