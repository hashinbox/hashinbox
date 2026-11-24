//! [`Counter`]: each request of a stored node counted in its
//! `network_endpoint_stats` row as it finishes.

use std::future::Future;

use tracing::Instrument as _;

use rpc::{Answer, Batch, BoxFuture, Client, Endpoint, Error, Outcome, RawJson, Request};

use crate::{db::Db, models::endpoint_stats, usecase::now};

/// `C`, each request it sends counted in node `endpoint_id`'s row, failed
/// or not (a node not at the block asked yet, `rpc::Error::is_not_ready`,
/// is no failure); one refused before sending is not. The write runs on its own
/// task, so the call never waits for it; a failed write is logged, and the
/// call's outcome is untouched. Put under `rpc::Split`, it counts HTTP
/// requests.
pub(crate) struct Counter<C> {
    inner: C,
    db: Db,
    endpoint_id: i64,
}

impl<C> Counter<C> {
    /// `inner`, counted as node `endpoint_id` in `db`.
    pub(crate) const fn new(inner: C, db: Db, endpoint_id: i64) -> Self {
        Self {
            inner,
            db,
            endpoint_id,
        }
    }

    async fn count<T: Outcome>(&self, sent: impl Future<Output = T>) -> T {
        let outcome = sent.await;
        let errors = outcome.errors();
        if errors.iter().any(Error::is_local) {
            return outcome;
        }
        let failed = errors.iter().any(|error| !error.is_not_ready());
        let (db, id, at) = (self.db.clone(), self.endpoint_id, now());
        // Not waited for: the call answers at once, the count lands after.
        let count = async move {
            let written = db
                .write(move |connection| {
                    Ok(endpoint_stats::db::add_call(connection, id, failed, at)?)
                })
                .await;
            if let Err(error) = written {
                let error = format!("{error:#}");
                tracing::error!(target: "endpoint.counted", node = id, error, "RPC node call not counted");
            }
        };
        tokio::spawn(count.instrument(tracing::Span::current()));
        outcome
    }
}

impl<C: Client> Client for Counter<C> {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<RawJson, Error>> {
        Box::pin(self.count(self.inner.request(request)))
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<Answer>, Error>> {
        Box::pin(self.count(self.inner.batch(batch)))
    }

    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(self.count(self.inner.broadcast(request)))
    }

    fn endpoint(&self) -> Option<&Endpoint> {
        self.inner.endpoint()
    }
}
