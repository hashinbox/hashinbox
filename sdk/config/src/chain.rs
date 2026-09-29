use std::{fmt, future::Future, sync::Arc};

use base::{BoxFuture, Network};
use json_rpc::Secrets;
use wallets::{Composition, Connection, SubmissionRegistrar};

use crate::{Error, ErrorKind};

/// One chain's configuration, with the chain crate that declared it erased.
///
/// A chain crate implements this for its own `Config<A>`, which is the whole
/// of registration: `sdk/config` depends on no chain crate, names no chain
/// type, and learns each one only through these three methods. It is **not**
/// the bundled chain trait `docs/CHAIN_LAYOUT.md` section 6 rejects — there is
/// no associated `Network`, `Source`, `Interpreter` or `KeyPair` here, and
/// nothing generic is written against it. It carries a section from the
/// document to the composition and stops.
///
/// The two phases never merge, exactly as [`wallets::Connection`] states them:
/// [`Self::accept`] touches nothing, and [`Self::connect`] contacts a node but
/// opens no storage (`docs/design/030 §8 (ex-028)` section 10.1).
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
}

impl<A: 'static, R: 'static> Chains<A, R> {
    /// No chains at all.
    #[must_use]
    pub fn new() -> Self {
        Self {
            chains: Vec::new(),
            notices: Vec::new(),
        }
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
    /// this, so a typo is refused before anything is deleted
    /// (`docs/design/019` section 5.3).
    #[must_use]
    pub fn networks(&self) -> Vec<Network> {
        self.chains
            .iter()
            .map(|(_, chain)| chain.network())
            .collect()
    }

    /// What every accepted chain asked an operator to be told, in section
    /// order. A library crate must not write to standard output, so printing
    /// them is the application's (`docs/design/030 §8 (ex-028)` section 8.4).
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
        let mut connections = Vec::with_capacity(self.chains.len());
        for (name, chain) in self.chains {
            connections.push(
                chain
                    .connect(secrets)
                    .await
                    .map_err(|error| error.in_section(&name))?,
            );
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
        let verification = async { self.connect(secrets).await.map_err(E::from) };
        Composition::verify_then_build(verification, open, repository, registrar).await
    }
}
