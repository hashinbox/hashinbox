//! The expire job: one per network, never finished. Every [`PERIOD`] it
//! forgets the network's pending transactions past their expiry, so the
//! funds and coins they held are spendable again.
//!
//! Its topic (`expire:chain:network`) conflicts with nothing else. A
//! broadcast job whose transaction is forgotten meanwhile fails
//! `outcome-unknown`.

use std::{sync::Arc, time::Duration};

use base::{Chain, Network};
use payments::Payments;
use serde_json::json;
use tasks::{Ack, Id, Job, Status, Store as _, Task, Topic, Worker};

use crate::{models::transaction, storage::Storage, workers::indexer::Target};

/// The job kind.
pub(crate) const KIND: &str = "expire";

/// How often each network's pending transactions are checked.
const PERIOD: Duration = Duration::from_secs(60);

/// Forgets every network's expired pending transactions.
pub(crate) struct Expire {
    payments: Arc<Payments>,
    storage: Storage,
}

impl Expire {
    pub(crate) const fn new(payments: Arc<Payments>, storage: Storage) -> Self {
        Self { payments, storage }
    }

    /// Makes sure every network has its expire job; a finished one starts
    /// over.
    pub(crate) async fn start(&self) -> anyhow::Result<()> {
        for network in self.payments.networks() {
            let target = Target {
                chain: network.chain.as_str().to_owned(),
                network: network.name.clone(),
            };
            let topic = Topic::new([format!("expire:{}:{}", target.chain, target.network)])
                .ok_or_else(|| anyhow::anyhow!("the network does not make a topic"))?;
            let job = Job {
                id: Id::from(format!("expire.{}.{}", target.chain, target.network)),
                ..Job::new(topic, KIND, json!(target))
            };
            match self.storage.get(&job.id).await? {
                None => self.storage.enqueue(job).await?,
                Some(kept) if matches!(kept.status, Status::Done | Status::Failed) => {
                    self.storage.update(job).await?;
                }
                Some(_) => {}
            }
        }
        Ok(())
    }
}

impl Worker for Expire {
    const KIND: &'static str = KIND;
    type Input = Target;
    type State = ();

    /// Forgets what expired, then runs again a [`PERIOD`] later; a failed
    /// write retries.
    async fn run(&self, task: Task<Target, ()>) -> Ack {
        let target = task.input();
        let network = Network::new(Chain::new(&target.chain), &target.network);
        let now = tasks::now();
        let Ok(at) = i64::try_from(now) else {
            return task.fail("now is above range");
        };
        let expired = self
            .storage
            .write(move |connection| Ok(transaction::db::expire(connection, &network, at)?))
            .await;
        match expired {
            Ok(_) => {
                let period = u64::try_from(PERIOD.as_millis()).unwrap_or(u64::MAX);
                task.again(now.saturating_add(period))
            }
            Err(error) => task.nack(format!("{error:#}")),
        }
    }
}
