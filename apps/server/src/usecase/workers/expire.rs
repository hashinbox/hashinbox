//! The expire job: one per indexed network, never finished. Every [`PERIOD`] it
//! forgets the network's pending transactions past their expiry, so the
//! funds and coins they held are spendable again.
//!
//! Its `before` enqueues one per indexed network, unique per network. Its
//! topic (`expire:chain:network`) conflicts with nothing else. A
//! broadcast job whose transaction is forgotten meanwhile fails
//! `outcome-unknown`.

use std::time::Duration;

use tasks::{Ack, Context, Policy, Store, StoreError, Topic, Worker, millis};

use super::{FOREVER, TIMEOUT, index::Target};
use crate::{db::Db, models::transaction};

/// The job kind.
pub(crate) const KIND: &str = "expire";

/// How often each network's pending transactions are checked.
const PERIOD: Duration = Duration::from_secs(60);

/// Forgets every network's expired pending transactions.
pub(crate) struct Expire {
    db: Db,
}

impl Expire {
    pub(crate) const fn new(db: Db) -> Self {
        Self { db }
    }
}

impl Worker for Expire {
    const KIND: &'static str = KIND;
    /// One per network, retried for ever.
    const POLICY: Policy = Policy {
        unique: true,
        timeout: TIMEOUT,
        retry: FOREVER,
        ..Policy::DEFAULT
    };
    type Input = Target;
    type State = ();

    /// `expire:chain:network`: it conflicts with nothing else.
    fn topic(target: &Target) -> Option<Topic> {
        Topic::new([format!("expire:{}", target.segment())])
    }

    /// Forgets what expired; a failed write retries.
    async fn run(&self, cx: Context<'_, Target, ()>) -> Ack {
        let network = cx.input().network();
        let Ok(at) = i64::try_from(tasks::now()) else {
            return Ack::Fail("now is above range".into());
        };
        let expired = self
            .db
            .write(move |connection| Ok(transaction::db::expire(connection, &network, at)?))
            .await;
        match expired {
            Ok(_) => Ack::Again(tasks::now().saturating_add(millis(PERIOD)), None),
            Err(error) => Ack::Retry(format!("{error:#}")),
        }
    }

    /// One job per indexed network.
    async fn before(&self, store: &dyn Store) -> Result<(), StoreError> {
        Target::enqueue_each::<Self>(&self.db, store).await
    }
}
