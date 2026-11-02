//! The trigger loop, driven by the indexer: each insert of a network's blocks
//! queues one trigger round of that network ([`super::Triggers`]) over the kept
//! blocks. Every network gets a round of every trigger at start (what changed
//! while the server was down) and on a slow fallback (a block notification
//! is a hint, never a guarantee).

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use base::{BlockHeader, Network};
use logging::{Level, Logger};
use payments::Payments;
use tasks::Store as _;
use tokio::sync::mpsc;

use super::Scope;
use crate::{models::Block, storage::Storage};

/// How often every trigger decides, whatever the blocks said.
const FALLBACK: Duration = Duration::from_secs(300);

/// Queues every network's triggers jobs as its blocks are kept.
pub(crate) struct Listen {
    payments: Arc<Payments>,
    storage: Storage,
    logger: Logger,
    /// Each insert's network and headers, from the block store's listener.
    blocks: mpsc::UnboundedReceiver<(Network, Vec<BlockHeader>)>,
}

impl Listen {
    pub(crate) fn new(
        payments: Arc<Payments>,
        storage: Storage,
        logger: Logger,
        blocks: mpsc::UnboundedReceiver<(Network, Vec<BlockHeader>)>,
    ) -> Self {
        Self {
            payments,
            storage,
            logger,
            blocks,
        }
    }

    /// Runs until dropped; a failure is logged and the loop goes on.
    pub(crate) async fn run(mut self) {
        self.everything().await;
        loop {
            tokio::select! {
                kept = self.blocks.recv() => {
                    let Some((network, headers)) = kept else { return };
                    // A burst (a catch-up) already queued is one round per network.
                    let mut inserts = BTreeMap::from([(network, headers)]);
                    while let Ok((network, headers)) = self.blocks.try_recv() {
                        inserts.entry(network).or_default().extend(headers);
                    }
                    for (network, headers) in inserts {
                        let blocks = headers
                            .iter()
                            .map(|header| Block::id(&network, &header.hash))
                            .collect::<anyhow::Result<Vec<_>>>();
                        match blocks {
                            Ok(blocks) => self.queue(&network, Some(blocks)).await,
                            Err(error) => self.warn(&network, &format!("{error:#}")),
                        }
                    }
                },
                () = tokio::time::sleep(FALLBACK) => self.everything().await,
            }
        }
    }

    /// A round of every trigger, per network.
    async fn everything(&self) {
        let networks: Vec<Network> = self.payments.networks().cloned().collect();
        for network in networks {
            self.queue(&network, None).await;
        }
    }

    /// Queues a round of `network` over `blocks` (every trigger when `None`).
    async fn queue(&self, network: &Network, blocks: Option<Vec<String>>) {
        let scope = Scope {
            chain: network.chain.as_str().to_owned(),
            network: network.name.clone(),
            blocks,
        };
        let queued = match scope.job() {
            Ok(job) => self.storage.enqueue(job).await.map_err(anyhow::Error::from),
            Err(error) => Err(error),
        };
        if let Err(error) = queued {
            self.warn(network, &format!("{error:#}"));
        }
    }

    fn warn(&self, network: &Network, message: &str) {
        self.logger.event(
            Level::Warn,
            "trigger.queue",
            message,
            &[
                ("chain", network.chain.as_str()),
                ("network", &network.name),
            ],
        );
    }
}
