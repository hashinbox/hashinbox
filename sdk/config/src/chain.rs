use std::{fmt, future::Future, sync::Arc};

use base::{BoxFuture, Network};
use json_rpc::{Secrets, Statistics};
use wallets::{Composition, Connection, SubmissionRegistrar};

use crate::{Error, ErrorKind};

/// Endpoint statistics grouped by network, in configured endpoint order.
/// Consumers label endpoints by ordinal; no URL or credential is retained.
pub type RpcStatistics = Vec<(Network, Vec<Statistics>)>;

/// One chain's configuration, erased at registration. The chain owns its
/// configuration and connection policy; this crate names no concrete chain.
/// Acceptance validates local settings; connection verifies node identity before
/// composition opens storage.
pub trait Chain<A, R>: Send + 'static {
    /// The network this section configures, read from the document alone: no
    /// node is contacted and no storage is opened.
    fn network(&self) -> Network;

    /// Rejects the section on its own, and states what accepting it commits
    /// this deployment to — the one line an operator must read before
    /// anything connects, when the chain has one.
    ///
    /// Bitcoin's confirmation depth is the example: it is the one safety
    /// number an operator chooses, and it used to be printed by the server,
    /// which had to name Bitcoin from outside to do it.
    ///
    /// # Errors
    ///
    /// Whatever the chain refuses about its own values. The section name is
    /// attached by the caller, because a chain does not know what it was
    /// registered as.
    fn accept(&self) -> Result<Option<String>, Error>;

    /// Verifies this chain's node identity. Nothing is opened and nothing is
    /// written, so a deployment that names the wrong node leaves no database
    /// behind.
    ///
    /// # Errors
    ///
    /// Whatever the chain reports when its node serves another network.
    fn connect<'a>(
        self: Box<Self>,
        secrets: &'a dyn Secrets,
        statistics: &'a mut Vec<Statistics>,
        logger: logging::Logger,
        transport: Option<crate::Transport>,
    ) -> BoxFuture<'a, Result<Box<dyn Connection<A, R>>, Error>>;
}

/// Every configured chain, accepted but holding nothing.
///
/// Built either by a [`Registry`](crate::Registry) from a document or by
/// [`Self::add`] from values in code: an embedder who writes
/// `chain_bitcoin::Config { .. }` by hand reaches the same composition as one
/// who reads a file, and links no parser to do it.
pub struct Chains<A, R> {
    chains: Vec<(String, Box<dyn Chain<A, R>>)>,
    notices: Vec<String>,
    logger: logging::Logger,
    transport: Option<crate::Transport>,
}

impl<A: 'static, R: 'static> Chains<A, R> {
    /// No chains at all.
    #[must_use]
    pub fn new() -> Self {
        Self {
            chains: Vec::new(),
            notices: Vec::new(),
            logger: logging::Logger::default(),
            transport: None,
        }
    }

    /// Injects one event sink for every configured chain and its RPC nodes.
    #[must_use]
    pub fn with_logger(mut self, logger: logging::Logger) -> Self {
        self.logger = logger;
        self
    }

    /// Uses a caller-owned transport for one configured network.
    /// Chain identity, token and protocol verification still run on this transport.
    ///
    /// # Errors
    /// Refuses configurations containing more than one network.
    pub fn with_transport(mut self, transport: crate::Transport) -> Result<Self, Error> {
        if self.chains.len() != 1 {
            return Err(Error::new(
                ErrorKind::Section,
                "transport injection requires one network",
            ));
        }
        self.transport = Some(transport);
        Ok(self)
    }

    /// Accepts `chain` under `name` and keeps whatever it asked to announce.
    ///
    /// This is the door that needs no document: a scenario or an embedder
    /// states its chains in Rust and reaches everything below.
    ///
    /// # Errors
    ///
    /// Whatever [`Chain::accept`] refuses, and [`ErrorKind::Section`] when a
    /// chain claims a network another already claimed — the contradiction
    /// `wallets::Composition` would otherwise report only after storage was
    /// open.
    pub fn add(
        &mut self,
        name: impl Into<String>,
        chain: impl Chain<A, R>,
    ) -> Result<&mut Self, Error> {
        self.push(name.into(), Box::new(chain))
    }

    pub(crate) fn push(
        &mut self,
        name: String,
        chain: Box<dyn Chain<A, R>>,
    ) -> Result<&mut Self, Error> {
        let network = chain.network();
        if self.networks().contains(&network) {
            return Err(Error::new(
                ErrorKind::Section,
                format!("{}/{} is already configured", network.chain, network.name),
            )
            .in_section(&name));
        }
        if let Some(notice) = chain.accept().map_err(|error| error.in_section(&name))? {
            self.notices.push(notice);
        }
        self.chains.push((name, chain));
        Ok(self)
    }

    /// How many chains this deployment configured.
    #[must_use]
    pub fn len(&self) -> usize {
        self.chains.len()
    }

    /// Whether nothing at all is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chains.is_empty()
    }

    /// Every configured chain network, read from the document alone: no node
    /// is contacted and no storage is opened.
    ///
    /// The offline rescan checks its `<chain>/<network>` argument against
    /// this, so a typo is refused before anything is deleted.
    #[must_use]
    pub fn networks(&self) -> Vec<Network> {
        self.chains
            .iter()
            .map(|(_, chain)| chain.network())
            .collect()
    }

    /// What every accepted chain asked an operator to be told, in section
    /// order. A library crate must not write to standard output, so printing
    /// them is the application's.
    #[must_use]
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    /// Phase one: every configured chain's node identity, checked in section
    /// order. Nothing is opened and nothing is written, and the caller decides
    /// when — and whether — this runs.
    ///
    /// # Errors
    ///
    /// The first chain whose node serves another network, named by its
    /// section.
    pub async fn connect(
        self,
        secrets: &dyn Secrets,
    ) -> Result<Vec<Box<dyn Connection<A, R>>>, Error> {
        self.connect_observed(secrets, &mut Vec::new()).await
    }

    async fn connect_observed(
        self,
        secrets: &dyn Secrets,
        statistics: &mut RpcStatistics,
    ) -> Result<Vec<Box<dyn Connection<A, R>>>, Error> {
        let mut connections = Vec::with_capacity(self.chains.len());
        for (name, chain) in self.chains {
            let network = chain.network();
            let mut endpoints = Vec::new();
            let logger = self
                .logger
                .with("chain", network.chain.to_string())
                .with("network", network.name.clone());
            let connection = chain
                .connect(secrets, &mut endpoints, logger, self.transport.clone())
                .await
                .map_err(|error| error.in_section(&name))?;
            connections.push(connection);
            statistics.push((network, endpoints));
        }
        Ok(connections)
    }
}

impl<A: 'static, R: 'static> fmt::Debug for Chains<A, R> {
    /// The sections that were accepted and the networks they named — never a
    /// section's contents, because an endpoint URL may carry a credential
    /// inline.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Chains")
            .field(
                "sections",
                &self.chains.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            )
            .field("networks", &self.networks())
            .finish()
    }
}

impl<A: 'static, R: 'static> Default for Chains<A, R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<A, R> Chains<A, R>
where
    A: Clone + Ord + Send + Sync + 'static,
    R: Clone + 'static,
{
    /// Both phases, in the one order that is safe: every node identity is
    /// verified, `open` runs only then, and every chain composes over the one
    /// repository `repository` makes out of the opened store.
    ///
    /// This is what an application would otherwise write out of
    /// `wallets::Composition::verify_then_build` by hand, once per scenario.
    ///
    /// # Errors
    ///
    /// The first identity failure, the storage failure that follows a
    /// successful verification, and whatever a chain reports while building
    /// its index and factories.
    pub async fn compose<S, E, O, F>(
        self,
        secrets: &dyn Secrets,
        open: O,
        repository: impl FnOnce(&S) -> Result<R, E>,
        registrar: &Arc<dyn SubmissionRegistrar>,
    ) -> Result<Composition<A>, E>
    where
        E: From<wallets::WalletError> + From<Error>,
        O: FnOnce() -> F,
        F: Future<Output = Result<S, E>>,
    {
        self.compose_with_statistics(secrets, open, repository, registrar)
            .await
            .map(|(composition, _)| composition)
    }

    /// Composes every chain and returns its endpoint counters beside the runtime.
    /// Statistics stay with deployment composition, outside the wallet capability layer.
    ///
    /// # Errors
    /// The same identity, storage and composition failures as [`Self::compose`].
    pub async fn compose_with_statistics<S, E, O, F>(
        self,
        secrets: &dyn Secrets,
        open: O,
        repository: impl FnOnce(&S) -> Result<R, E>,
        registrar: &Arc<dyn SubmissionRegistrar>,
    ) -> Result<(Composition<A>, RpcStatistics), E>
    where
        E: From<wallets::WalletError> + From<Error>,
        O: FnOnce() -> F,
        F: Future<Output = Result<S, E>>,
    {
        let mut statistics = Vec::new();
        let verification = async {
            self.connect_observed(secrets, &mut statistics)
                .await
                .map_err(E::from)
        };
        let composition =
            Composition::verify_then_build(verification, open, repository, registrar).await?;
        Ok((composition, statistics))
    }
}
