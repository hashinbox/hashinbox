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

use std::{future::Future, sync::Arc};

use base::{BoxFuture, Units};
use indexing::{Indexers, Indexing, Network, Pending};
use wallets::{SubmissionRegistrar, WalletError, WalletErrorKind, WalletFactory, Wallets};

use crate::config::Error;

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
/// factory under its asset tag, and every declared asset's unit under its
/// identity. Each index carries its own chain's poll cadence.
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
}

impl<A> Composition<A>
where
    A: Clone + Ord + Send + Sync + 'static,
{
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
        let mut wallets = Wallets::new();
        let mut units = Units::default();
        for chain in chains {
            let ComposedChain {
                index,
                pending: held,
                factories,
                units: declared,
            } = chain
                .compose(repository.clone(), registrar)
                .await
                .map_err(E::from)?;
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
