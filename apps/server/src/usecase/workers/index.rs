//! The index job: one per `enabled` network, never finished. Each run
//! steps the network's indexer (`base::Indexer::next`), writes each step to
//! its `network_index` row, made by the first step that knows the tip (tip,
//! confirmed position `tip - confirmations`, last kept block, and what the
//! counters grew by since the last write), and asks to run
//! again: at once while behind, one interval later once at the tip. A
//! network whose every node is disabled stops (`Ack::Idle`), asking
//! nothing; enabling or adding a node queues it again
//! (`usecase::networks`). A failed
//! step runs again an interval later with its error as the job's `reason`;
//! so does a step that went on past a failed gap close (an index left
//! behind, its balance as last settled); the next run without either clears
//! it.
//!
//! At the tip it queues a triggers job, unique per network: none while the
//! network's last one is still pending or running, so every trigger decides
//! each interval, never on a chain still catching up, and never twice on the
//! same balances.
//!
//! Its `before` enqueues one per indexed network when its queue starts,
//! unique per network: a repeat is a duplicate. Its topic (`index:chain:network`) conflicts with
//! nothing else: indexing runs beside triggers and broadcasts; each of them
//! reads or writes in one transaction.

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use base::{Chain, Network, Next, Stats};
use serde::{Deserialize, Serialize};
use tasks::{Ack, Context, Policy, Store, StoreError, Topic, Worker, millis};
use tracing::Instrument as _;

use super::{FOREVER, trigger::Round};
use crate::{
    db::Db,
    models,
    usecase::{Served as _, network_span, tasks::Queue},
};

/// The job kind.
pub(crate) const KIND: &str = "index";

/// Steps per run: at most this many batches before the job yields.
const STEPS: usize = 10;

/// The longest run: above one slow step (~3 min, docs/issues.md T1), so a
/// run makes progress; past it the run is dropped and retried.
const TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// The longest one step may take, well below [`TIMEOUT`]: past it the run
/// is retried, the step's timeout its reason.
const STEP: Duration = Duration::from_secs(60);

/// The network an index job steps.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Target {
    pub(crate) chain: String,
    pub(crate) network: String,
}

impl From<&Network> for Target {
    fn from(network: &Network) -> Self {
        Self {
            chain: network.chain.as_str().to_owned(),
            network: network.name.clone(),
        }
    }
}

impl Target {
    /// Its network.
    pub(crate) fn network(&self) -> Network {
        Network::new(Chain::new(&self.chain), &self.network)
    }

    /// `chain:network`: one topic segment naming the network.
    pub(crate) fn segment(&self) -> String {
        format!("{}:{}", self.chain, self.network)
    }

    /// Every `enabled` network: those the server indexes.
    ///
    /// # Errors
    /// The read, as a store failure.
    pub(crate) async fn indexed(db: &Db) -> Result<Vec<Self>, StoreError> {
        let rows = db.read(models::network::db::find_all).await;
        let rows = rows.map_err(|error| StoreError::Backend(format!("{error:#}")))?;
        let indexed = rows.iter().filter(|network| network.enabled);
        Ok(indexed.map(|network| Self::from(&network.key())).collect())
    }

    /// Enqueues `W`'s job of every indexed network in `store`.
    ///
    /// # Errors
    /// The read or an enqueue.
    pub(crate) async fn enqueue_each<W: Worker<Input = Self>>(
        db: &Db,
        store: &dyn Store,
    ) -> Result<(), StoreError> {
        for target in Self::indexed(db).await? {
            W::enqueue(store, &target).await?;
        }
        Ok(())
    }
}

/// Steps every indexed network's indexer.
pub(crate) struct Index {
    container: Arc<base::Container<crate::usecase::Transport>>,
    db: Db,
    queue: Queue,
    /// Each indexer's stats as last written: a step writes what grew since.
    saved: Mutex<BTreeMap<Network, Stats>>,
    /// The longest one step may take: [`STEP`].
    step: Duration,
}

impl Index {
    pub(crate) fn new(
        container: Arc<base::Container<crate::usecase::Transport>>,
        db: Db,
        queue: Queue,
    ) -> Self {
        Self {
            container,
            db,
            queue,
            saved: Mutex::default(),
            step: STEP,
        }
    }

    /// Its steps cut at `step` (tests).
    #[cfg(test)]
    pub(crate) const fn with_step(mut self, step: Duration) -> Self {
        self.step = step;
        self
    }
}

impl Worker for Index {
    const KIND: &'static str = KIND;
    /// Four networks step at once, each run within [`TIMEOUT`], retried for
    /// ever: the job never ends, one per network. Its schedule is its own:
    /// each run answers `Again` at its network's interval.
    const POLICY: Policy = Policy {
        unique: true,
        concurrency: NonZeroUsize::new(4).unwrap(),
        timeout: TIMEOUT,
        retry: FOREVER,
        ..Policy::DEFAULT
    };
    type Input = Target;
    type State = ();

    /// `index:chain:network`: it conflicts with nothing else.
    fn topic(target: &Target) -> Option<Topic> {
        Topic::new([format!("index:{}", target.segment())])
    }

    /// Up to [`STEPS`] steps of the indexer, each written to its
    /// `network_index` row, each cut at [`STEP`]. At the tip it queues a
    /// triggers job (none while
    /// one is unfinished) and runs again an interval later; still behind, it
    /// runs again at once. A failed step or row write is retried by the
    /// queue, which waits longer after each failure in a row (a node
    /// answering 429, say, gets time to recover), its error the job's
    /// `reason`: the job never ends. A step that failed only a gap close
    /// goes on, the close's error the run's `reason`. The run is in its
    /// network's span; the executor logs it as `job.run`.
    async fn run(&self, cx: Context<'_, Target, ()>) -> Ack {
        let span = network_span(&cx.input().network());
        self.steps(cx).instrument(span).await
    }

    /// One job per indexed network.
    async fn before(&self, store: &dyn Store) -> Result<(), StoreError> {
        Target::enqueue_each::<Self>(&self.db, store).await
    }
}

impl Index {
    /// A run's steps: see [`Worker::run`].
    async fn steps(&self, cx: Context<'_, Target, ()>) -> Ack {
        let target = cx.input().clone();
        let network = target.network();
        let now = tasks::now();
        let service = match self.container.service(&network) {
            Ok(service) => service,
            Err(error) => return Ack::Fail(format!("{error:#}")),
        };
        let (indexer, confirmations) = (service.indexer.clone(), service.confirmations);
        let later = now.saturating_add(millis(indexer.interval()));
        // No node to ask: the job stops; enabling a node queues it again.
        if !self.container.has_node(&network) {
            return Ack::Idle;
        }
        let mut closed = None;
        for _ in 0..STEPS {
            let Ok(next) = tokio::time::timeout(self.step, indexer.next()).await else {
                let seconds = self.step.as_secs_f64();
                return Ack::Retry(format!("an index step timed out after {seconds} s"));
            };
            let after = indexer.stats();
            let before = self.saved().get(&network).cloned().unwrap_or_default();
            if after.failures > before.failures {
                closed.clone_from(&after.error);
            }
            // No tip yet: no row to write.
            if let Some(step) =
                models::NetworkIndex::step(&before, &after, confirmations, tasks::now())
            {
                let at = network.clone();
                let saved = self
                    .db
                    .write(move |connection| {
                        models::network_index::db::save(connection, &at, &step)
                    })
                    .await;
                if let Err(error) = saved {
                    return Ack::Retry(format!("{error:#}"));
                }
            }
            self.saved().insert(network.clone(), after);
            match next {
                Ok(Next::Indexed(_) | Next::Reorged(_)) => {}
                Ok(Next::Idle) => {
                    let queued = Round::enqueue(&self.queue, &target).await;
                    let reason = queued.err().map(|error| error.to_string());
                    return Ack::Again(later, reason.or(closed));
                }
                // Its last node went during the step: stopped as above.
                Err(_) if !self.container.has_node(&network) => return Ack::Idle,
                Err(error) => return Ack::Retry(format!("{error:#}")),
            }
        }
        Ack::Again(now, closed)
    }

    fn saved(&self) -> std::sync::MutexGuard<'_, BTreeMap<Network, Stats>> {
        self.saved.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
