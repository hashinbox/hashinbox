use std::{collections::BTreeMap, fmt, future::Future, sync::Arc};

use base::{BoxFuture, Network};
use json_rpc::{Secrets, Statistics, Swap};
use wallets::SubmissionRegistrar;

use crate::{ChainNodes, Composition, Connection, Error, ErrorKind, Rpc};

/// Endpoint statistics grouped by network, in configured endpoint order.
/// Consumers label endpoints by ordinal; no URL or credential is retained.
pub type RpcStatistics = Vec<(Network, Vec<Statistics>)>;

/// What [`Chains::compose_available`] built: one composition of every chain
/// whose node passed verification, beside what the others left behind.
pub struct Startup<A: Ord, R> {
    /// Every verified chain, composed over the one repository.
    pub composition: Composition<A>,
    /// Each verified network's endpoint counters, for an application's
    /// health report.
    pub statistics: RpcStatistics,
    /// Every configured network, composed or skipped, in section order: the
    /// transport it was composed over and the chain that checks a candidate
    /// node list for it ([`ChainNodes::verify`]).
    pub networks: Vec<ChainNodes<A, R>>,
    /// Why each skipped section's node failed verification; every error
    /// names its section.
    pub skipped: Vec<Error>,
}

/// One chain's configuration, erased at registration. The chain owns its
/// configuration and connection policy; this crate names no concrete chain.
/// Acceptance validates local settings; connection verifies node identity before
/// composition opens storage, through the same check a running deployment
/// applies to a replacement node ([`Self::verify`]).
// linter:disable rust/trait-method-count -- `connect` runs `verify`; replacements reuse that check.
pub trait Chain<A, R>: Send + Sync + 'static {
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

    /// Runs this chain's node identity checks over `transport` — exactly the
    /// ones [`Self::connect`] runs, and nothing else. Nothing is opened and
    /// nothing is written.
    ///
    /// # Errors
    ///
    /// Whatever the chain reports when the node serves another network or
    /// lacks what this deployment needs of it.
    fn verify<'a>(&'a self, transport: &'a crate::Transport) -> BoxFuture<'a, Result<(), Error>>;

    /// Builds this chain's transport (or uses `transport`), verifies its node
    /// ([`Self::verify`]) and returns what composes it. Nothing is opened and
    /// nothing is written.
    ///
    /// # Errors
    ///
    /// Whatever the chain reports when its node serves another network.
    fn connect<'a>(
        &'a self,
        secrets: &'a dyn Secrets,
        statistics: &'a mut Vec<Statistics>,
        logger: logging::Logger,
        transport: Option<crate::Transport>,
    ) -> BoxFuture<'a, Result<Box<dyn Connection<A, R>>, Error>>;
}

/// What connecting the verified chains leaves behind for a [`Startup`].
struct Inventory<A, R> {
    statistics: RpcStatistics,
    networks: Vec<ChainNodes<A, R>>,
}

impl<A, R> Default for Inventory<A, R> {
    fn default() -> Self {
        Self {
            statistics: Vec::new(),
            networks: Vec::new(),
        }
    }
}

/// Every configured chain, accepted but holding nothing.
///
/// Built either by a [`Registry`](crate::Registry) from a document or by
/// [`Self::add`] from values in code: an embedder who writes
/// `chain_bitcoin::Config { .. }` by hand reaches the same composition as one
/// who reads a file, and links no parser to do it.
pub struct Chains<A, R> {
    chains: Vec<(String, Arc<dyn Chain<A, R>>)>,
    notices: Vec<String>,
    logger: logging::Logger,
    /// Caller-owned transports, by the network they serve.
    transports: BTreeMap<Network, crate::Transport>,
    /// The `rpc` of every chain a [`Registry`](crate::Registry) parsed from a
    /// document, by network.
    endpoints: BTreeMap<Network, Endpoints>,
}

/// One parsed section's `rpc`: decoded, and its endpoint documents as written.
pub(crate) struct Endpoints {
    pub(crate) rpc: Rpc,
    pub(crate) documents: Vec<serde_json::Value>,
}

impl Endpoints {
    /// The `rpc` of `section`, when it has one.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Endpoints`] when it does not decode; nothing is quoted.
    pub(crate) fn of(section: &serde_json::Value) -> Result<Option<Self>, Error> {
        let Some(rpc) = section.get("rpc") else {
            return Ok(None);
        };
        let documents = rpc
            .get("endpoints")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let rpc = serde_json::from_value(rpc.clone())
            .map_err(|_| Error::new(ErrorKind::Endpoints, "its rpc settings do not decode"))?;
        Ok(Some(Self { rpc, documents }))
    }

    /// Endpoint documents as the nodes they describe; nothing is quoted.
    pub(crate) fn decode(
        documents: &[serde_json::Value],
    ) -> Result<Vec<json_rpc::Endpoint>, Error> {
        documents
            .iter()
            .map(|document| {
                serde_json::from_value(document.clone()).map_err(|_| {
                    Error::new(
                        ErrorKind::Endpoints,
                        "an RPC node is a URL or an object of url, headers and bounds",
                    )
                })
            })
            .collect()
    }
}

impl<A: 'static, R: 'static> Chains<A, R> {
    /// No chains at all.
    #[must_use]
    pub fn new() -> Self {
        Self {
            chains: Vec::new(),
            notices: Vec::new(),
            logger: logging::Logger::default(),
            transports: BTreeMap::new(),
            endpoints: BTreeMap::new(),
        }
    }

    /// Injects one event sink for every configured chain and its RPC nodes.
    #[must_use]
    pub fn with_logger(mut self, logger: logging::Logger) -> Self {
        self.logger = logger;
        self
    }

    /// Uses a caller-owned transport for `network` instead of the one its
    /// section's `rpc` would build. The chain's identity, token and protocol
    /// verification still run, on this transport; every other network keeps
    /// its own.
    ///
    /// The caller owns what it injected, its endpoint counters included:
    /// [`Startup::statistics`] lists no endpoint for an injected network.
    /// An application that replaces its nodes while running injects a
    /// transport it can repoint.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Section`] when no accepted section configures `network`.
    pub fn with_transport_for(
        mut self,
        network: &Network,
        transport: crate::Transport,
    ) -> Result<Self, Error> {
        if !self.networks().contains(network) {
            return Err(Error::new(
                ErrorKind::Section,
                format!("{}/{} is not configured", network.chain, network.name),
            ));
        }
        self.transports.insert(network.clone(), transport);
        Ok(self)
    }

    /// Connects each network of `endpoints` to those endpoint documents in
    /// place of its section's, keeping the section's repetition: an
    /// application that keeps its nodes elsewhere (stored) hands them back.
    /// Nothing is parsed again; a network no section configures is left out,
    /// and its documents are kept by whoever stored them.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Endpoints`] when a document is not an endpoint, naming the
    /// section and never quoting it.
    pub fn with_endpoints(
        mut self,
        endpoints: &BTreeMap<Network, Vec<serde_json::Value>>,
    ) -> Result<Self, Error> {
        let names: BTreeMap<Network, String> = self
            .chains
            .iter()
            .map(|(name, chain)| (chain.network(), name.clone()))
            .collect();
        for (network, configured) in &mut self.endpoints {
            let (Some(documents), Some(name)) = (endpoints.get(network), names.get(network)) else {
                continue;
            };
            configured.rpc.endpoints =
                Endpoints::decode(documents).map_err(|error| error.in_section(name))?;
            configured.documents.clone_from(documents);
        }
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
    /// [`Composition`] would otherwise report only after storage was
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
        self.push_parsed(name, chain, None)
    }

    /// [`Self::push`] for a chain parsed from a document section whose `rpc`
    /// is `endpoints`: composition builds its transport as a [`Swap`].
    pub(crate) fn push_parsed(
        &mut self,
        name: String,
        chain: Box<dyn Chain<A, R>>,
        endpoints: Option<Endpoints>,
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
        if let Some(endpoints) = endpoints {
            self.endpoints.insert(network, endpoints);
        }
        self.chains.push((name, Arc::from(chain)));
        Ok(self)
    }

    /// The endpoint documents every chain parsed from a document connects to,
    /// as written (`${NAME}` unresolved, credentials included), by network:
    /// what an application that keeps its nodes elsewhere stores.
    pub fn endpoints(&self) -> impl Iterator<Item = (&Network, &[serde_json::Value])> {
        self.endpoints
            .iter()
            .map(|(network, endpoints)| (network, endpoints.documents.as_slice()))
    }

    /// The transport `network` connects over: a caller-injected one, else,
    /// for a chain parsed from a document, a [`Swap`] over its section's
    /// nodes (returned beside it), else none, and the chain builds its own.
    fn transport(
        &self,
        network: &Network,
        secrets: &dyn Secrets,
        logger: &logging::Logger,
    ) -> Result<(Option<crate::Transport>, Option<Arc<Swap>>), Error> {
        if let Some(transport) = self.transports.get(network) {
            return Ok((Some(transport.clone()), None));
        }
        let Some(endpoints) = self.endpoints.get(network) else {
            return Ok((None, None));
        };
        let (transport, statistics) = endpoints.rpc.logged(secrets, logger)?;
        let swap = Arc::new(Swap::new(transport, statistics));
        Ok((Some(swap.clone()), Some(swap)))
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
        let mut observed = Inventory::default();
        self.connect_observed(secrets, &mut observed, None).await
    }

    /// `chain`'s connection over the transport [`Self::transport`] builds for
    /// it, a failure named by its section `name`; with the swap that
    /// transport is, when the section's nodes built one.
    async fn connect_chain(
        &self,
        name: &str,
        chain: &Arc<dyn Chain<A, R>>,
        secrets: &dyn Secrets,
        endpoints: &mut Vec<Statistics>,
        logger: &logging::Logger,
    ) -> (Result<Box<dyn Connection<A, R>>, Error>, Option<Arc<Swap>>) {
        let (transport, swap) = match self.transport(&chain.network(), secrets, logger) {
            Ok(built) => built,
            Err(error) => return (Err(error.in_section(name)), None),
        };
        let connected = chain
            .connect(secrets, endpoints, logger.clone(), transport)
            .await
            .map_err(|error| error.in_section(name));
        (connected, swap)
    }

    /// Every chain's connection, checked in section order. With `skipped`,
    /// a chain whose node fails verification is recorded there and left out;
    /// without it, that failure ends the check. Every chain, connected or
    /// not, is recorded in `observed` with the transport it was composed over.
    async fn connect_observed(
        self,
        secrets: &dyn Secrets,
        observed: &mut Inventory<A, R>,
        mut skipped: Option<&mut Vec<Error>>,
    ) -> Result<Vec<Box<dyn Connection<A, R>>>, Error> {
        let mut connections = Vec::with_capacity(self.chains.len());
        for (name, chain) in &self.chains {
            let network = chain.network();
            let mut endpoints = Vec::new();
            let logger = self
                .logger
                .with("chain", network.chain.to_string())
                .with("network", network.name.clone());
            let (connected, swap) = self
                .connect_chain(name, chain, secrets, &mut endpoints, &logger)
                .await;
            let repeat = self
                .endpoints
                .get(&network)
                .map(|ends| ends.rpc.repeat.clone());
            let served = connected.is_ok().then_some(swap).flatten();
            observed.networks.push(ChainNodes::new(
                name.clone(),
                chain.clone(),
                repeat.unwrap_or_default(),
                logger,
                served.clone(),
            ));
            let connection = match (connected, skipped.as_deref_mut()) {
                (Ok(connection), _) => connection,
                (Err(error), Some(skipped)) => {
                    skipped.push(error);
                    continue;
                }
                (Err(error), None) => return Err(error),
            };
            connections.push(connection);
            if let Some(swap) = served {
                endpoints.extend(swap.statistics());
            }
            observed.statistics.push((network, endpoints));
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
    /// [`Composition::verify_then_build`] by hand, once per scenario.
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
        let verification = async { self.connect(secrets).await.map_err(E::from) };
        Composition::verify_then_build(verification, open, repository, registrar).await
    }

    /// Composes every chain whose node passes verification and skips the
    /// rest, so one unreachable or misconfigured node costs only its own
    /// network. Storage opens only after every check has returned, as in
    /// [`Self::compose`]; with every chain skipped, the composition is empty.
    ///
    /// # Errors
    ///
    /// The storage failure, and whatever a verified chain reports while
    /// building its index and factories. A failed verification is never an
    /// error here: it is in [`Startup::skipped`].
    pub async fn compose_available<S, E, O, F>(
        self,
        secrets: &dyn Secrets,
        open: O,
        repository: impl FnOnce(&S) -> Result<R, E>,
        registrar: &Arc<dyn SubmissionRegistrar>,
    ) -> Result<Startup<A, R>, E>
    where
        E: From<wallets::WalletError> + From<Error>,
        O: FnOnce() -> F,
        F: Future<Output = Result<S, E>>,
    {
        let mut observed = Inventory::default();
        let mut skipped = Vec::new();
        let verification = async {
            self.connect_observed(secrets, &mut observed, Some(&mut skipped))
                .await
                .map_err(E::from)
        };
        let composition =
            Composition::verify_then_build(verification, open, repository, registrar).await?;
        Ok(Startup {
            composition,
            statistics: observed.statistics,
            networks: observed.networks,
            skipped,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use base::ChainName;
    use json_rpc::{Batch, CallResult, Client, Request};

    use super::*;

    /// A chain that reports, as its refusal, which transport it was handed.
    struct Probe(&'static str);

    impl Chain<(), ()> for Probe {
        fn network(&self) -> Network {
            Network::new(ChainName::new("probe"), self.0)
        }

        fn accept(&self) -> Result<Option<String>, Error> {
            Ok(None)
        }

        fn verify<'a>(
            &'a self,
            _transport: &'a crate::Transport,
        ) -> BoxFuture<'a, Result<(), Error>> {
            Box::pin(async { Ok(()) })
        }

        fn connect<'a>(
            &'a self,
            _secrets: &'a dyn Secrets,
            _statistics: &'a mut Vec<Statistics>,
            _logger: logging::Logger,
            transport: Option<crate::Transport>,
        ) -> BoxFuture<'a, Result<Box<dyn Connection<(), ()>>, Error>> {
            Box::pin(async move {
                let answer = match transport {
                    Some(transport) => transport
                        .request(Request::new("identity", serde_json::json!([])))
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .and_then(|raw| raw.deserialize::<String>().ok())
                        .unwrap_or_default(),
                    None => "own".to_owned(),
                };
                Err(Error::refused(answer))
            })
        }
    }

    /// A node that answers every call with its own name.
    struct Named(&'static str);

    impl Client for Named {
        fn request<'a>(
            &'a self,
            _request: Request,
        ) -> json_rpc::BoxFuture<'a, Result<CallResult, json_rpc::Error>> {
            let name = json_rpc::RawJson::from_serializable(&self.0).expect("a name");
            Box::pin(async move { Ok(Ok(name)) })
        }

        fn batch<'a>(
            &'a self,
            _batch: Batch,
        ) -> json_rpc::BoxFuture<'a, Result<Vec<CallResult>, json_rpc::Error>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn chains() -> Chains<(), ()> {
        let mut chains = Chains::new();
        for name in ["one", "two", "three"] {
            chains.add(name, Probe(name)).expect("distinct networks");
        }
        chains
    }

    /// Each network reaches exactly the transport injected for it, and a
    /// network without one builds its own.
    #[tokio::test]
    async fn each_network_connects_over_its_own_injected_transport() {
        let network = |name| Network::new(ChainName::new("probe"), name);
        let chains = chains()
            .with_transport_for(&network("one"), Arc::new(Named("first")))
            .expect("one is configured")
            .with_transport_for(&network("three"), Arc::new(Named("third")))
            .expect("three is configured");
        let mut observed = Inventory::default();
        let mut skipped = Vec::new();
        let connected = chains
            .connect_observed(&BTreeMap::new(), &mut observed, Some(&mut skipped))
            .await
            .expect("every refusal is skipped");
        assert!(connected.is_empty());
        let answers: Vec<String> = skipped.iter().map(ToString::to_string).collect();
        assert_eq!(answers, ["one: first", "two: own", "three: third"]);
    }

    #[test]
    fn a_transport_for_an_unconfigured_network_is_refused() {
        let error = chains()
            .with_transport_for(
                &Network::new(ChainName::new("probe"), "four"),
                Arc::new(Named("x")),
            )
            .expect_err("no section configures it");
        assert_eq!(error.kind, ErrorKind::Section);
        assert_eq!(error.to_string(), "probe/four is not configured");
    }

    /// A section's chain: its network, its `rpc`, and whether its node passes.
    #[derive(serde::Deserialize)]
    struct Section {
        network: String,
        rpc: Rpc,
        #[serde(default)]
        refuses: bool,
    }

    impl Chain<(), ()> for Section {
        fn network(&self) -> Network {
            Network::new(ChainName::new("section"), self.network.as_str())
        }

        fn accept(&self) -> Result<Option<String>, Error> {
            self.rpc.validate().map(|()| None)
        }

        fn verify<'a>(
            &'a self,
            _transport: &'a crate::Transport,
        ) -> BoxFuture<'a, Result<(), Error>> {
            Box::pin(async move {
                if self.refuses {
                    return Err(Error::refused("the node serves another network"));
                }
                Ok(())
            })
        }

        fn connect<'a>(
            &'a self,
            _secrets: &'a dyn Secrets,
            _statistics: &'a mut Vec<Statistics>,
            _logger: logging::Logger,
            transport: Option<crate::Transport>,
        ) -> BoxFuture<'a, Result<Box<dyn Connection<(), ()>>, Error>> {
            Box::pin(async move {
                let transport = transport.ok_or_else(|| Error::refused("no transport"))?;
                self.verify(&transport).await?;
                Ok(Box::new(Verified) as Box<dyn Connection<(), ()>>)
            })
        }
    }

    /// A verified section; nothing composes it here.
    struct Verified;

    impl Connection<(), ()> for Verified {
        fn compose<'a>(
            self: Box<Self>,
            _repository: (),
            _registrar: &'a Arc<dyn SubmissionRegistrar>,
        ) -> BoxFuture<'a, Result<crate::ComposedChain<()>, Error>> {
            Box::pin(async { Err(Error::refused("not composed here")) })
        }
    }

    fn sections(value: serde_json::Value) -> Chains<(), ()> {
        let mut registry = crate::Registry::new();
        registry
            .register::<Section>("one")
            .register::<Section>("two");
        let sections = serde_json::from_value(value).expect("a section table");
        registry.chains(&sections).expect("accepted")
    }

    fn section(name: &str) -> Network {
        Network::new(ChainName::new("section"), name)
    }

    /// Every network parsed from a document connects over a swap built from
    /// its nodes — the stored ones where an application handed them back —
    /// and one whose nodes build no transport is skipped with the reason,
    /// never dropped.
    #[tokio::test]
    async fn parsed_networks_connect_over_swaps_of_their_effective_nodes() {
        let chains = sections(serde_json::json!({
            "one": {"network": "one", "rpc": {"endpoints": ["http://127.0.0.1:9"]}},
            "two": {"network": "two", "rpc": {"endpoints": ["http://${MISSING}.invalid"]}},
        }));
        let stored = BTreeMap::from([
            (
                section("one"),
                vec![
                    serde_json::json!("http://127.0.0.1:10"),
                    serde_json::json!("http://127.0.0.1:11"),
                ],
            ),
            (
                section("gone"),
                vec![serde_json::json!("http://127.0.0.1:12")],
            ),
        ]);
        let chains = chains.with_endpoints(&stored).expect("decodes");
        let documents: Vec<(&Network, usize)> = chains
            .endpoints()
            .map(|(network, documents)| (network, documents.len()))
            .collect();
        assert_eq!(documents, [(&section("one"), 2), (&section("two"), 1)]);

        let mut observed = Inventory::default();
        let mut skipped = Vec::new();
        let connected = chains
            .connect_observed(&BTreeMap::new(), &mut observed, Some(&mut skipped))
            .await
            .expect("every failure is skipped");
        assert_eq!(connected.len(), 1);
        let reasons: Vec<String> = skipped.iter().map(ToString::to_string).collect();
        assert_eq!(
            reasons,
            ["two: a configured endpoint's environment variable is unavailable"]
        );
        let one = &observed.networks[0];
        let swap = one.transport().expect("one composed over a swap");
        assert_eq!(
            swap.statistics().len(),
            2,
            "the stored nodes, not the file's"
        );
        assert_eq!(observed.statistics[0].1.len(), 2);
        assert!(
            observed.networks[1].transport().is_none(),
            "two was skipped"
        );

        let refused = Chains::<(), ()>::new()
            .with_endpoints(&BTreeMap::new())
            .expect("nothing to decode");
        assert!(refused.is_empty());
        let malformed = sections(serde_json::json!({
            "one": {"network": "one", "rpc": {"endpoints": ["http://127.0.0.1:9"]}},
        }))
        .with_endpoints(&BTreeMap::from([(
            section("one"),
            vec![serde_json::json!(7)],
        )]))
        .expect_err("not an endpoint");
        assert_eq!(malformed.kind, ErrorKind::Endpoints);
        assert!(malformed.to_string().starts_with("one: "), "{malformed}");
    }

    /// A candidate is built with the section's repetition and checked by the
    /// chain alone; a node list that builds nothing and a node the chain
    /// refuses are told apart, both naming the section.
    #[tokio::test]
    async fn a_candidate_is_checked_by_its_chain() {
        let chains = sections(serde_json::json!({
            "one": {"network": "one", "rpc": {"endpoints": ["http://127.0.0.1:9"]}},
            "two": {"network": "two", "refuses": true,
                    "rpc": {"endpoints": ["http://127.0.0.1:9"]}},
        }));
        let mut observed = Inventory::default();
        let mut skipped = Vec::new();
        chains
            .connect_observed(&BTreeMap::new(), &mut observed, Some(&mut skipped))
            .await
            .expect("skipped, not failed");
        let (one, two) = (&observed.networks[0], &observed.networks[1]);
        let candidate = || vec![json_rpc::Endpoint::new("http://127.0.0.1:10")];
        let (_, statistics) = one
            .verify(candidate(), &BTreeMap::new())
            .await
            .expect("the chain accepts it");
        assert_eq!(statistics.len(), 1);
        let empty = one
            .verify(Vec::new(), &BTreeMap::new())
            .await
            .err()
            .expect("none");
        assert_eq!(empty.kind, ErrorKind::Endpoints);
        let unset = vec![json_rpc::Endpoint::new("http://${MISSING}.invalid")];
        let unset = one
            .verify(unset, &BTreeMap::new())
            .await
            .err()
            .expect("unset");
        assert_eq!(unset.kind, ErrorKind::Endpoints);
        assert!(unset.to_string().starts_with("one: "), "{unset}");
        let refused = two
            .verify(candidate(), &BTreeMap::new())
            .await
            .err()
            .expect("refused");
        assert_eq!(refused.kind, ErrorKind::Unverified);
        assert_eq!(refused.to_string(), "two: the node serves another network");
    }
}
