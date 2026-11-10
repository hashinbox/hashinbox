//! The index job: one per `network_index` row, never finished. Each run
//! steps the network's indexer (`base::Indexer::next`), writes each step to
//! the row (tip, confirmed position `tip - confirmations`, last kept block,
//! and what the counters grew by since the last write), and asks to run
//! again: at once while behind, one interval later once at the tip. A failed
//! step runs again an interval later with its error as the job's `reason`;
//! the next run that does not fail clears it.
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
use payments::Payments;
use serde::{Deserialize, Serialize};
use tasks::{Ack, Context, Policy, Store, StoreError, Topic, Worker, millis};

use super::{FOREVER, trigger::Round};
use crate::{db::Db, models, usecase::tasks::Queue};

/// The job kind.
pub(crate) const KIND: &str = "index";

/// Steps per run: at most this many batches before the job yields.
const STEPS: usize = 10;

/// The longest run: above one slow step (~3 min, docs/issues.md T1), so a
/// run makes progress; past it the run is dropped and retried.
const TIMEOUT: Duration = Duration::from_secs(5 * 60);

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

    /// Every network with a `network_index` row.
    ///
    /// # Errors
    /// The read, as a store failure.
    pub(crate) async fn indexed(db: &Db) -> Result<Vec<Self>, StoreError> {
        let rows = db.read(models::network_index::db::find_all).await;
        let rows = rows.map_err(|error| StoreError::Backend(format!("{error:#}")))?;
        Ok(rows.keys().map(Self::from).collect())
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
    /// `network_index` row. At the tip it queues a triggers job (none while
    /// one is unfinished) and runs again an interval later; still behind, it
    /// runs again at once. A failed step or row write waits an interval and
    /// tries again, its error the job's `reason`: the job never ends.
    async fn run(&self, cx: Context<'_, Target, ()>) -> Ack {
        let target = cx.input().clone();
        let network = target.network();
        let now = tasks::now();
        let service = match self.payments.service(&network) {
            Ok(service) => service,
            Err(error) => return Ack::Fail(format!("{error:#}")),
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
                return Ack::Again(later, Some(format!("{error:#}")));
            }
            self.saved().insert(network.clone(), after);
            match next {
                Ok(Next::Indexed(_) | Next::Reorged(_)) => {}
                Ok(Next::Idle) => {
                    let queued = Round::enqueue(&self.queue, &target).await;
                    let reason = queued.err().map(|error| error.to_string());
                    return Ack::Again(later, reason);
                }
                Err(error) => return Ack::Again(later, Some(format!("{error:#}"))),
            }
        }
        Ack::Again(now, None)
    }

    /// One job per indexed network.
    async fn before(&self, store: &dyn Store) -> Result<(), StoreError> {
        Target::enqueue_each::<Self>(&self.db, store).await
    }
}

impl Index {
    fn saved(&self) -> std::sync::MutexGuard<'_, BTreeMap<Network, Stats>> {
        self.saved.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
