//! The index job: one per `network_index` row, never finished. Each run
//! steps the network's indexer (`base::Indexer::next`), writes each step to
//! the row (tip, confirmed position `tip - confirmations`, last kept block,
//! and what the counters grew by since the last write), and asks to run
//! again: at once while behind, one interval later once at the tip. A failed
//! step runs again an interval later with its error as the job's `reason`;
//! the next run that does not fail clears it.
//!
//! At the tip it queues a triggers job: every trigger decides each interval,
//! never on a chain still catching up.
//!
//! Its topic (`index:chain:network`) conflicts with nothing else: indexing
//! runs beside triggers and broadcasts; each of them reads or writes in one
//! transaction.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use base::{Chain, Network, Next, Stats};
use payments::Payments;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tasks::{Ack, Id, Job, Status, Store as _, Task, Topic, Worker};

use super::trigger::Scope;
use crate::{db::Db, models, usecase::tasks::Queue};

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

/// Steps every indexed network's indexer.
pub(crate) struct Index {
    payments: Arc<Payments>,
    db: Db,
    queue: Queue,
    /// Each indexer's stats as last written: a step writes what grew since.
    saved: Mutex<BTreeMap<Network, Stats>>,
}

impl Index {
    pub(crate) fn new(payments: Arc<Payments>, db: Db, queue: Queue) -> Self {
        Self {
            payments,
            db,
            queue,
            saved: Mutex::default(),
        }
    }

    /// Makes sure every `network_index` row has its index job, pending now;
    /// a finished one (a Retry ran out) starts over.
    pub(crate) async fn start(&self) -> anyhow::Result<()> {
        let indexed = self.db.read(models::network_index::db::find_all).await?;
        for network in indexed.keys() {
            let target = Target {
                chain: network.chain.as_str().to_owned(),
                network: network.name.clone(),
            };
            let name = format!("{}:{}", target.chain, target.network);
            let topic = Topic::new([format!("index:{name}")])
                .ok_or_else(|| anyhow::anyhow!("the network does not make a topic"))?;
            let job = Job {
                id: job_id(&target.chain, &target.network),
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

    /// Up to [`STEPS`] steps of the indexer, each written to its
    /// `network_index` row. At the tip it queues a triggers job and runs again an interval
    /// later; still behind, it runs again at once. A failed step or row
    /// write waits an interval and tries again, its error the job's
    /// `reason`: the job never ends.
    async fn run(&self, task: Task<Target, ()>) -> Ack {
        let target = task.input().clone();
        let network = Network::new(Chain::new(&target.chain), &target.network);
        let now = tasks::now();
        let service = match self.payments.service(&network) {
            Ok(service) => service,
            Err(error) => return task.fail(format!("{error:#}")),
        };
        let (indexer, confirmations) = (service.indexer.clone(), service.confirmations);
        let later = now.saturating_add(millis(indexer.interval()));
        for _ in 0..STEPS {
            let next = indexer.next().await;
            let after = indexer.stats();
            let before = self.saved().get(&network).cloned().unwrap_or_default();
            let step = models::NetworkIndex::step(&before, &after, confirmations, tasks::now());
            let at = network.clone();
            let saved = self
                .db
                .write(move |connection| models::network_index::db::save(connection, &at, &step))
                .await;
            if let Err(error) = saved {
                return task.again_because(later, format!("{error:#}"));
            }
            self.saved().insert(network.clone(), after);
            match next {
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
                Err(error) => return task.again_because(later, format!("{error:#}")),
            }
        }
        task.again(now)
    }
}

impl Index {
    fn saved(&self) -> std::sync::MutexGuard<'_, BTreeMap<Network, Stats>> {
        self.saved.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The index job of `chain`'s `network`: one per network.
pub(crate) fn job_id(chain: &str, network: &str) -> Id {
    Id::from(format!("index.{chain}.{network}"))
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
