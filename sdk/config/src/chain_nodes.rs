//! One configured network after startup: its chain, the transport it was
//! composed over, and the check a replacement node list passes.

use std::{fmt, sync::Arc};

use base::Network;
use json_rpc::{Endpoint, Secrets, Statistics, Swap};

use crate::{Chain, Error, ErrorKind, Repetition, Rpc, Transport};

/// One network of a [`crate::Startup`], composed or skipped.
///
/// A composed network's chain holds [`Self::transport`], a [`Swap`]: an
/// application replaces its nodes while it runs by checking candidates with
/// [`Self::verify`] and installing the transport that passed with
/// [`Swap::replace`]. A skipped network has no transport, and a candidate that
/// passes is for the next start. Nothing is parsed again for a candidate.
pub struct ChainNodes<A, R> {
    section: String,
    network: Network,
    chain: Arc<dyn Chain<A, R>>,
    /// The section's repetition, which a candidate keeps.
    repeat: Repetition,
    logger: logging::Logger,
    transport: Option<Arc<Swap>>,
}

impl<A: 'static, R: 'static> ChainNodes<A, R> {
    pub(crate) fn new(
        section: String,
        chain: Arc<dyn Chain<A, R>>,
        repeat: Repetition,
        logger: logging::Logger,
        transport: Option<Arc<Swap>>,
    ) -> Self {
        Self {
            network: chain.network(),
            section,
            chain,
            repeat,
            logger,
            transport,
        }
    }

    /// The network this section configures.
    #[must_use]
    pub const fn network(&self) -> &Network {
        &self.network
    }

    /// The transport the composed chain calls through; `None` when the
    /// network was skipped, or its transport was injected by the caller.
    #[must_use]
    pub const fn transport(&self) -> Option<&Arc<Swap>> {
        self.transport.as_ref()
    }

    /// Builds the transport `endpoints` describe, with the section's
    /// repetition, and runs the chain's own identity check over it
    /// ([`Chain::verify`]). Returns that transport and its nodes' counters,
    /// ready for [`Swap::replace`]; nothing is opened or written. The caller
    /// bounds how long this may take.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Endpoints`] when the endpoints build no transport (none,
    /// or an unset `${NAME}`), and [`ErrorKind::Unverified`] with the chain's
    /// reason when the node does not prove this network. Every error names
    /// the section.
    pub async fn verify(
        &self,
        endpoints: Vec<Endpoint>,
        secrets: &dyn Secrets,
    ) -> Result<(Transport, Vec<Statistics>), Error> {
        let rpc = Rpc {
            endpoints,
            repeat: self.repeat.clone(),
        };
        let in_section = |error: Error| error.in_section(&self.section);
        rpc.validate().map_err(in_section)?;
        let (transport, statistics) = rpc.logged(secrets, &self.logger).map_err(in_section)?;
        self.chain.verify(&transport).await.map_err(|error| {
            Error::new(ErrorKind::Unverified, error.message).in_section(&self.section)
        })?;
        Ok((transport, statistics))
    }
}

impl<A, R> fmt::Debug for ChainNodes<A, R> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChainNodes")
            .field("section", &self.section)
            .field("network", &self.network)
            .field("composed", &self.transport.is_some())
            .finish()
    }
}
