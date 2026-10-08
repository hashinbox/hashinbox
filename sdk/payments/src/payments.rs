//! [`Payments`]: the container. Parse or build each chain's configuration,
//! attach a store slot by slot, and [`Builder::start`] verifies every node,
//! composes, drives the index and waits for readiness.

use std::{collections::BTreeMap, fmt, sync::Arc};

use base::{BoxFuture, Network, Units};
use futures_channel::mpsc::UnboundedReceiver;
use indexing::{
    Birthday, Change, Driver, Holder, IndexError, IndexErrorKind, IndexStore, IndexedBatch,
    Indexers, Location, Notifying, OutputKey, OutputPage, OutputRequest, OutputStore, Pending,
    PurgedRows, TransactionRef, TransactionStore, WalletRegistry, Watch, WatchProgress, WatchStore,
    Watching as _,
};
use serde::de::DeserializeOwned;
use tokio::{sync::watch, task::JoinHandle};
use wallets::{SubmissionRegistrar, Submissions, Wallet, Wallets};

use crate::{
    Networks,
    config::{Chain, Chains, Document, Error, Registry, Sections},
};

/// The store every chain is composed over: one view per trait slot of the
/// caller's instance(s), never a wrapper around the store itself.
///
/// The two required views drive the engine; each optional one reaches only the
/// machine that needs it. An optional slot left empty answers its one consumer
/// with an error naming the missing `with_*` call. Built only by
/// [`Builder::start`].
#[derive(Clone)]
pub struct Split {
    watches: Arc<dyn WatchStore>,
    index: Arc<dyn IndexStore>,
    outputs: Option<Arc<dyn OutputStore>>,
    transactions: Option<Arc<dyn TransactionStore>>,
}

/// Why an optional slot was asked for and never attached.
fn unattached(call: &str) -> IndexError {
    IndexError::new(
        IndexErrorKind::InvalidRequest,
        format!("this store slot was never attached to Payments ({call})"),
        false,
    )
}

impl WatchStore for Split {
    fn add<'a>(&'a self, watches: Vec<Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
        self.watches.add(watches)
    }

    fn all<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<WatchProgress>, IndexError>> {
        self.watches.all(network)
    }

    fn forget<'a>(
        &'a self,
        network: &'a Network,
        watch: &'a Watch,
    ) -> BoxFuture<'a, Result<(), IndexError>> {
        self.watches.forget(network, watch)
    }
}

impl IndexStore for Split {
    fn checkpoint<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<base::BlockRef>, IndexError>> {
        self.index.checkpoint(network)
    }

    fn commit<'a>(
        &'a self,
        batch: IndexedBatch,
    ) -> BoxFuture<'a, Result<Vec<OutputKey>, IndexError>> {
        self.index.commit(batch)
    }

    fn purge<'a>(&'a self, network: &'a Network) -> BoxFuture<'a, Result<PurgedRows, IndexError>> {
        self.index.purge(network)
    }
}

impl OutputStore for Split {
    fn list<'a>(&'a self, request: OutputRequest) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
        match &self.outputs {
            Some(outputs) => outputs.list(request),
            None => Box::pin(async { Err(unattached("with_output_store")) }),
        }
    }
}

impl TransactionStore for Split {
    fn locate<'a>(
        &'a self,
        transaction: &'a TransactionRef,
    ) -> BoxFuture<'a, Result<Location, IndexError>> {
        match &self.transactions {
            Some(transactions) => transactions.locate(transaction),
            None => Box::pin(async { Err(unattached("with_transaction_store")) }),
        }
    }
}

/// Why [`Builder::start`] (or a builder step) refused.
#[derive(Debug)]
pub enum StartError {
    /// The document or a chain's section.
    Config(Error),
    /// A required slot ([`Builder::with_store`]) was never attached.
    MissingStore,
    /// Composing a verified chain, or registering its wallet factories.
    Compose(String),
    /// The driver stopped before every network settled.
    Stopped,
}

impl fmt::Display for StartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => write!(formatter, "{error}"),
            Self::MissingStore => formatter.write_str("no store was attached (with_store)"),
            Self::Compose(message) => write!(formatter, "composition failed: {message}"),
            Self::Stopped => formatter.write_str("synchronization stopped before readiness"),
        }
    }
}

impl std::error::Error for StartError {}

impl From<Error> for StartError {
    fn from(error: Error) -> Self {
        Self::Config(error)
    }
}

impl From<wallets::WalletError> for StartError {
    fn from(error: wallets::WalletError) -> Self {
        Self::Compose(error.to_string())
    }
}

/// One container under construction; every step is one decision.
pub struct Builder<A: Ord> {
    sections: Option<Sections>,
    registry: Registry<A, Split>,
    coded: Vec<(String, Box<dyn Chain<A, Split>>)>,
    endpoints: Option<BTreeMap<Network, Vec<serde_json::Value>>>,
    logger: logging::Logger,
    watches: Option<Arc<dyn WatchStore>>,
    index: Option<Arc<dyn IndexStore>>,
    index_override: bool,
    outputs: Option<Arc<dyn OutputStore>>,
    transactions: Option<Arc<dyn TransactionStore>>,
    registry_slot: Option<Arc<dyn WalletRegistry>>,
}

/// Every configured chain composed, its index driven, its operational handle.
pub struct Payments<A: Ord> {
    /// Every configured asset's wallet factory, keyed by asset tag.
    pub wallets: Wallets<A>,
    /// Every composed network's index.
    pub index: Arc<Indexers>,
    /// Every composed network's blocks above its confirmed line, to complete
    /// a repository's balance and history reads.
    pub pending: Vec<(Network, Arc<dyn Pending>)>,
    /// Every declared asset's unit.
    pub units: Units,
    /// The operational handle: nodes, pause, resume, rescan, health.
    pub networks: Networks<A>,
    /// The wallet registry slot, when one was attached; the engine never
    /// calls it.
    pub registry: Option<Arc<dyn WalletRegistry>>,
    /// Lines each accepted chain asked an operator to read.
    pub notices: Vec<String>,
    /// Why each section whose node failed verification was skipped.
    pub skipped: Vec<Error>,
    notifying: Option<Arc<Notifying<Arc<dyn IndexStore>>>>,
    submissions: Arc<Submissions>,
    stop: watch::Sender<bool>,
    sync: JoinHandle<()>,
}

impl<A> Payments<A>
where
    A: Clone + Ord + Send + Sync + 'static,
{
    /// A builder over a TOML document of chain sections, one per name a
    /// later [`Builder::with`] registers.
    ///
    /// # Errors
    ///
    /// The document does not decode into named sections.
    #[cfg(feature = "toml")]
    pub fn from_toml(text: &str) -> Result<Builder<A>, Error> {
        let sections: Sections = Document::toml(text).parse()?;
        Ok(Builder::with_sections(Some(sections)))
    }

    /// A builder over a JSON document of chain sections.
    ///
    /// # Errors
    ///
    /// The document does not decode into named sections.
    pub fn from_json(bytes: &[u8]) -> Result<Builder<A>, Error> {
        let sections: Sections = Document::json(bytes)?.parse()?;
        Ok(Builder::with_sections(Some(sections)))
    }

    /// A builder with no document: every chain is added in code
    /// ([`Builder::chain`]).
    #[must_use]
    pub fn builder() -> Builder<A> {
        Builder::with_sections(None)
    }

    /// Registers `wallet` for indexing as `id` from `birthday`.
    ///
    /// # Errors
    ///
    /// The index's refusal ([`indexing::Watching::watch`]).
    pub async fn watch(
        &self,
        id: &str,
        wallet: &Arc<dyn Wallet>,
        birthday: Birthday,
    ) -> Result<Watch, IndexError> {
        let holder: &dyn Holder = wallet.as_ref();
        self.index.watch(id, holder, birthday).await
    }

    /// A stream of every [`Change`] committed from now on: one per watch a
    /// commit advanced. `None` when [`Builder::with_index_store`] replaced
    /// the container's notifying index slot.
    #[must_use]
    pub fn changes(&self) -> Option<UnboundedReceiver<Change>> {
        self.notifying.as_ref().map(|notifying| notifying.changes())
    }

    /// Stops in the one safe order: no new sends, submission registration
    /// closed and drained while indexing can still reconcile them, then
    /// indexing stopped and joined.
    ///
    /// # Errors
    ///
    /// A submission or the driver task panicked.
    pub async fn shutdown(self) -> Result<(), tokio::task::JoinError> {
        self.networks.reset_health();
        self.wallets.close();
        self.submissions.close();
        let drained = self.submissions.drain().await;
        let _ = self.stop.send(true);
        let joined = self.sync.await;
        drained.and(joined)
    }
}

impl<A> Builder<A>
where
    A: Clone + Ord + Send + Sync + 'static,
{
    fn with_sections(sections: Option<Sections>) -> Self {
        Self {
            sections,
            registry: Registry::new(),
            coded: Vec::new(),
            endpoints: None,
            logger: logging::Logger::default(),
            watches: None,
            index: None,
            index_override: false,
            outputs: None,
            transactions: None,
            registry_slot: None,
        }
    }

    /// Registers `C` as the chain that parses the section called `name`.
    #[must_use]
    pub fn with<C>(mut self, name: &str) -> Self
    where
        C: Chain<A, Split> + DeserializeOwned,
    {
        self.registry.register::<C>(name);
        self
    }

    /// Adds one chain's configuration built in code, named by its network.
    #[must_use]
    pub fn chain(mut self, chain: impl Chain<A, Split>) -> Self {
        let network = chain.network();
        let name = format!("{}/{}", network.chain, network.name);
        self.coded.push((name, Box::new(chain)));
        self
    }

    /// Logs every chain's RPC traffic and the driver's passes to `logger`.
    #[must_use]
    pub fn with_logger(mut self, logger: logging::Logger) -> Self {
        self.logger = logger;
        self
    }

    /// Composes each named network over these endpoint documents instead of
    /// its section's (an application that keeps its nodes elsewhere).
    #[must_use]
    pub fn with_endpoints(mut self, endpoints: BTreeMap<Network, Vec<serde_json::Value>>) -> Self {
        self.endpoints = Some(endpoints);
        self
    }

    /// The required ports, both views of one caller instance.
    #[must_use]
    pub fn with_store<S>(mut self, store: Arc<S>) -> Self
    where
        S: WatchStore + IndexStore + 'static,
    {
        self.watches = Some(store.clone());
        if !self.index_override {
            self.index = Some(store);
        }
        self
    }

    /// The optional output slot (Bitcoin's coin selection).
    #[must_use]
    pub fn with_output_store<S: OutputStore + 'static>(mut self, store: Arc<S>) -> Self {
        self.outputs = Some(store);
        self
    }

    /// The optional transaction slot (Solana's sender recovery).
    #[must_use]
    pub fn with_transaction_store<S: TransactionStore + 'static>(mut self, store: Arc<S>) -> Self {
        self.transactions = Some(store);
        self
    }

    /// The optional wallet registry slot, held for the application.
    #[must_use]
    pub fn with_wallet_registry<S: WalletRegistry + 'static>(mut self, store: Arc<S>) -> Self {
        self.registry_slot = Some(store);
        self
    }

    /// Replaces the container's notifying index slot with `store`;
    /// [`Payments::changes`] then answers `None`.
    #[must_use]
    pub fn with_index_store<S: IndexStore + 'static>(mut self, store: Arc<S>) -> Self {
        self.index = Some(store);
        self.index_override = true;
        self
    }

    /// Every configured chain with its endpoint documents as written, before
    /// [`Self::with_endpoints`] replaced them.
    ///
    /// # Errors
    ///
    /// A section no chain registered for, or one its chain refuses.
    pub fn endpoints(&self) -> Result<BTreeMap<Network, Vec<serde_json::Value>>, Error> {
        Ok(self
            .chains()?
            .endpoints()
            .map(|(network, documents)| (network.clone(), documents.to_vec()))
            .collect())
    }

    fn chains(&self) -> Result<Chains<A, Split>, Error> {
        match &self.sections {
            Some(sections) => self.registry.chains(sections),
            None => Ok(Chains::new()),
        }
    }

    /// Verifies every node, composes each chain whose node passed over the
    /// attached slots, drives the index, and returns once every network
    /// settled. A chain whose node fails is skipped ([`Payments::skipped`]).
    ///
    /// # Errors
    ///
    /// A refused section, no [`Self::with_store`], a composition failure, or
    /// the driver stopping before readiness.
    pub async fn start(mut self) -> Result<Payments<A>, StartError> {
        let mut chains = self.chains()?.with_logger(self.logger.clone());
        for (name, chain) in std::mem::take(&mut self.coded) {
            chains.push(name, chain)?;
        }
        if let Some(endpoints) = &self.endpoints {
            chains = chains.with_endpoints(endpoints)?;
        }
        let notices = chains.notices().to_vec();
        let endpoints = chains
            .endpoints()
            .map(|(network, documents)| {
                crate::config::Endpoints::decode(documents).map(|nodes| (network.clone(), nodes))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let (Some(watches), Some(index)) = (self.watches, self.index) else {
            return Err(StartError::MissingStore);
        };
        let notifying = (!self.index_override).then(|| Arc::new(Notifying::new(index.clone())));
        let index: Arc<dyn IndexStore> = match &notifying {
            Some(notifying) => notifying.clone(),
            None => index,
        };
        let split = Split {
            watches,
            index: index.clone(),
            outputs: self.outputs,
            transactions: self.transactions,
        };
        let submissions = Arc::new(Submissions::new());
        let registrar: Arc<dyn SubmissionRegistrar> = submissions.clone();
        let startup = chains
            .compose_available(
                || std::future::ready(Ok::<_, StartError>(split)),
                |split: &Split| Ok::<_, StartError>(split.clone()),
                &registrar,
            )
            .await?;
        let composition = startup.composition;
        let driver = Driver::new(composition.index.clone()).with_logger(self.logger);
        let mut readiness = driver.readiness();
        let networks = Networks::new(
            startup.networks,
            endpoints,
            composition.wallets.clone(),
            composition.index.clone(),
            index,
            &driver,
        );
        let (stop, stopped) = watch::channel(false);
        let sync = tokio::spawn(driver.run(stopped));
        readiness.settled().await.ok_or(StartError::Stopped)?;
        Ok(Payments {
            wallets: composition.wallets,
            index: composition.index,
            pending: composition.pending,
            units: composition.units,
            networks,
            registry: self.registry_slot,
            notices,
            skipped: startup.skipped,
            notifying,
            submissions,
            stop,
            sync,
        })
    }
}
