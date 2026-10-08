//! Offline operator actions. Stop the server first; these never connect to RPC,
//! start the driver or bind a listener. Application keys are never purged.

use crate::config::{AnyError, Config};
use base::{ChainName, Network};
use indexing::IndexStore as _;
use std::{ffi::OsString, fmt};

/// One explicit operator action over the configured file.
pub(crate) enum Operation {
    Rescan(Network),
}

impl Operation {
    pub(crate) fn parse(
        arguments: impl Iterator<Item = OsString>,
    ) -> Result<Option<Self>, AnyError> {
        let arguments = arguments.collect::<Vec<_>>();
        match arguments.as_slice() {
            [] => Ok(None),
            [flag, target] if flag == "--rescan" => {
                let text = target.to_str().ok_or("network argument is not UTF-8")?;
                Ok(Some(Self::Rescan(Self::network(text)?)))
            }
            _ => Err(crate::USAGE.into()),
        }
    }

    fn network(text: &str) -> Result<Network, AnyError> {
        let malformed = || format!("operator command takes one <chain>/<network> argument: {text}");
        let (chain, name) = text.split_once('/').ok_or_else(malformed)?;
        if chain.trim().is_empty() || name.trim().is_empty() || name.contains('/') {
            return Err(malformed().into());
        }
        Ok(Network::new(ChainName::new(chain), name))
    }

    pub(crate) async fn run(&self, config: &Config) -> Result<(), AnyError> {
        let configured = config.chains()?.networks();
        let Self::Rescan(network) = self;
        if !configured.contains(network) {
            let listed = configured
                .iter()
                .map(|network| format!("{}/{}", network.chain, network.name))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "{self} is not a configured network; this file configures {listed}"
            )
            .into());
        }
        let store = config.storage.open().await?;
        let repository = store.repository();
        match self {
            Self::Rescan(network) => {
                let removed = repository.purge(network).await?;
                println!(
                    "removed {} checkpoint, {} history, {} movement, \
                     {} block-credit, and {} output rows \
                     from {self}; watches and application keys preserved",
                    removed.checkpoint,
                    removed.history,
                    removed.movements,
                    removed.credits,
                    removed.outputs
                );
            }
        }
        Ok(())
    }
}

impl fmt::Display for Operation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rescan(network) => {
                write!(formatter, "{}/{}", network.chain, network.name)
            }
        }
    }
}
