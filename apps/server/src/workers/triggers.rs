//! The trigger loop, driven by the indexer: each insert of a network's blocks
//! settles the runs it confirms, then the triggers it concerns decide, with the ones held back
//! (busy, cooling down, below a threshold) asked again. A decided plan
//! becomes one run and its send jobs, kept together.
//!
//! Every trigger decides once at start (what changed while the server was
//! down) and on a slow fallback (a block notification is a hint, never a
//! guarantee).

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base::{Address, BlockHeader, Network};
use logging::{Level, Logger};
use payments::Payments;
use tokio::sync::mpsc;

use super::send::{Payment, SendIntent};
use crate::{
    models::{Json, TriggerRun},
    storage::Storage,
    trigger::{self, Plan, Sources, Verdict},
};

/// How often every trigger decides, whatever the blocks said.
const FALLBACK: Duration = Duration::from_secs(300);

/// Runs every network's triggers as its blocks are kept.
pub(crate) struct Triggers {
    payments: Arc<Payments>,
    storage: Storage,
    logger: Logger,
    /// Each insert's network and headers, from the block store's listener.
    blocks: mpsc::UnboundedReceiver<(Network, Vec<BlockHeader>)>,
    /// Per network, the triggers held back at their last decision.
    held: BTreeMap<Network, BTreeSet<String>>,
}

impl Triggers {
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
            held: BTreeMap::new(),
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
                        if let Err(error) = self.kept(&network, headers).await {
                            self.warn(Some(&network), "trigger.block", &format!("{error:#}"));
                        }
                    }
                },
                () = tokio::time::sleep(FALLBACK) => self.everything().await,
            }
        }
    }

    /// Every network: settle, then every trigger decides.
    async fn everything(&mut self) {
        let networks: Vec<Network> = self.payments.networks().cloned().collect();
        for network in networks {
            if let Err(error) = self.settle(&network).await {
                self.warn(Some(&network), "trigger.settle", &format!("{error:#}"));
            }
            if let Err(error) = self.decide(&network, None).await {
                self.warn(Some(&network), "trigger.decide", &format!("{error:#}"));
            }
        }
    }

    /// `network` kept `headers`: its runs settle, then the triggers they
    /// touched and the held ones decide.
    async fn kept(&mut self, network: &Network, headers: Vec<BlockHeader>) -> anyhow::Result<()> {
        let touched = network.clone();
        let mut triggers = self
            .storage
            .read(move |connection| trigger::touched(connection, &touched, &headers))
            .await?;
        triggers.extend(self.settle(network).await?);
        triggers.extend(self.held.get(network).into_iter().flatten().cloned());
        if !triggers.is_empty() {
            self.decide(network, Some(triggers)).await?;
        }
        Ok(())
    }

    /// Settles `network`'s done runs; the triggers that may act again.
    async fn settle(&self, network: &Network) -> anyhow::Result<Vec<String>> {
        let (network, now) = (network.clone(), now()?);
        self.storage
            .write(move |connection| trigger::run::settle(connection, &network, now))
            .await
    }

    /// The triggers of `network` in `only` (all when `None`) decide; a plan
    /// starts a run.
    async fn decide(
        &mut self,
        network: &Network,
        only: Option<BTreeSet<String>>,
    ) -> anyhow::Result<()> {
        let now = now()?;
        let service = self.payments.service(network)?;
        let sources = Sources::new(service.assets.clone(), service.wallets.factory.fees(), now);
        let (decided, scope) = (network.clone(), only.clone());
        let verdicts = self
            .storage
            .read(move |connection| trigger::decide(connection, &decided, scope.as_ref(), &sources))
            .await?;
        let held = self.held.entry(network.clone()).or_default();
        if only.is_none() {
            held.clear();
        }
        let (mut plans, mut refused) = (Vec::new(), Vec::new());
        for (id, verdict) in verdicts {
            held.remove(&id);
            match verdict {
                Ok(Verdict::Act(plan)) => plans.push((id, plan)),
                Ok(Verdict::Held) => {
                    held.insert(id);
                }
                Ok(Verdict::Rest) => {}
                Err(error) => refused.push(format!("{id}: {error:#}")),
            }
        }
        for message in refused {
            self.warn(Some(network), "trigger.refused", &message);
        }
        for (id, plan) in plans {
            self.start(network, &id, &plan, now).await?;
        }
        Ok(())
    }

    /// Starts a run of trigger `id` with the send jobs of `plan`.
    async fn start(
        &self,
        network: &Network,
        id: &str,
        plan: &Plan,
        now: i64,
    ) -> anyhow::Result<()> {
        let jobs: Vec<_> = intents(network, plan).iter().map(SendIntent::job).collect();
        let run = TriggerRun {
            jobs: Json(jobs.iter().map(|job| job.id.to_string()).collect()),
            ..TriggerRun::new(id, now)
        };
        let kept = jobs.clone();
        self.storage
            .write(move |connection| trigger::run::start(connection, &run, &kept, now))
            .await?;
        for job in jobs {
            self.storage.announce(job);
        }
        Ok(())
    }

    fn warn(&self, network: Option<&Network>, event: &str, message: &str) {
        let (chain, name) = network.map_or(("", ""), |network| {
            (network.chain.as_str(), network.name.as_str())
        });
        self.logger.event(
            Level::Warn,
            event,
            message,
            &[("chain", chain), ("network", name)],
        );
    }
}

/// Unix ms.
fn now() -> anyhow::Result<i64> {
    Ok(i64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

/// `plan` as send intents: one per sender, its transfers in plan order.
fn intents(network: &Network, plan: &Plan) -> Vec<SendIntent> {
    let mut by_sender: BTreeMap<&Address, Vec<Payment>> = BTreeMap::new();
    for transfer in &plan.transfers {
        by_sender.entry(&transfer.from).or_default().push(Payment {
            to: transfer.to.value.clone(),
            amount: transfer.amount.to_string(),
        });
    }
    by_sender
        .into_iter()
        .map(|(from, transfers)| SendIntent {
            chain: network.chain.as_str().to_owned(),
            network: network.name.clone(),
            address: from.value.clone(),
            asset: plan.asset.clone(),
            transfers,
        })
        .collect()
}
