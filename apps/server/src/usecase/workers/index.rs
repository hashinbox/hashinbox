//! The index job: one per network, never finished. Each run steps the
//! network's indexer (`base::Indexer::next`) and asks to run again: at once
//! while behind, one interval later once at the tip.
//!
//! At the tip it queues a triggers job: every trigger decides each interval,
//! never on a chain still catching up.
//!
//! Its topic (`index:chain:network`) conflicts with nothing else: indexing
//! runs beside triggers and broadcasts; each of them reads or writes in one
//! transaction.

use std::{sync::Arc, time::Duration};

use base::{Chain, Network, Next};
use payments::Payments;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tasks::{Ack, Id, Job, Status, Store as _, Task, Topic, Worker};

use super::trigger::Scope;
use crate::usecase::tasks::Queue;

/// The job kind.
pub(crate) const KIND: &str = "index";

/// Steps per run: at most this many batches before the job yields.
const STEPS: usize = 10;

/// The network an index job steps.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Target {
    pub(crate) chain: String,
    pub(crate) network: String,
}

/// Steps every network's indexer.
pub(crate) struct Index {
    payments: Arc<Payments>,
    queue: Queue,
}

impl Index {
    pub(crate) const fn new(payments: Arc<Payments>, queue: Queue) -> Self {
        Self { payments, queue }
    }

    /// Makes sure every network has its index job, pending now; a finished
    /// one (a Retry ran out) starts over.
    pub(crate) async fn start(&self) -> anyhow::Result<()> {
        for network in self.payments.networks() {
            let target = Target {
                chain: network.chain.as_str().to_owned(),
                network: network.name.clone(),
            };
            let name = format!("{}:{}", target.chain, target.network);
            let topic = Topic::new([format!("index:{name}")])
                .ok_or_else(|| anyhow::anyhow!("the network does not make a topic"))?;
            let job = Job {
                id: Id::from(format!("index.{}.{}", target.chain, target.network)),
                ..Job::new(topic, KIND, json!(target))
            };
            match self.queue.get(&job.id).await? {
                None => self.queue.enqueue(job).await?,
                Some(kept) if matches!(kept.status, Status::Done | Status::Failed) => {
                    self.queue.update(job).await?;
                }
                Some(_) => {}
            }
        }
        Ok(())
    }
}

impl Worker for Index {
    const KIND: &'static str = KIND;
    type Input = Target;
    type State = ();

    /// Up to [`STEPS`] steps of the indexer. At the tip it queues a triggers
    /// job and runs again an interval later; still behind, it runs again at
    /// once. A failed step (in the indexer's stats) waits an interval and
    /// tries again: the job never ends.
    async fn run(&self, task: Task<Target, ()>) -> Ack {
        let target = task.input().clone();
        let network = Network::new(Chain::new(&target.chain), &target.network);
        let now = tasks::now();
        let indexer = match self.payments.service(&network) {
            Ok(service) => service.indexer.clone(),
            Err(error) => return task.fail(format!("{error:#}")),
        };
        let later = now.saturating_add(millis(indexer.interval()));
        for _ in 0..STEPS {
            match indexer.next().await {
                Ok(Next::Indexed(_) | Next::Reorged(_)) => {}
                Ok(Next::Idle) => {
                    let scope = Scope {
                        chain: target.chain,
                        network: target.network,
                    };
                    // Not queued, the next interval queues one.
                    if let Ok(job) = scope.job() {
                        let _ = self.queue.enqueue(job).await;
                    }
                    return task.again(later);
                }
                Err(_) => return task.again(later),
            }
        }
        task.again(now)
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
