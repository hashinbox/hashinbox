use std::{
    fmt,
    sync::{Arc, Mutex, PoisonError},
};

use crate::{Batch, BoxFuture, CallError, CallResult, Client, Error, Request, Statistics};

/// A client whose nodes can be replaced while every holder keeps the same
/// handle.
///
/// A chain composed over a `Swap` never learns that its nodes changed:
/// [`Self::replace`] installs another transport, with that transport's
/// per-node [`Statistics`], in one step. Each call takes the transport current
/// when it starts and finishes on it, so a call in flight during a replacement
/// is answered by the node it was sent to. Every [`Client`] method is
/// forwarded, a broadcast included, so swapping never narrows one.
///
/// `Debug` shows the node count and nothing else.
pub struct Swap(Mutex<Transport>);

/// The transport calls start on now, and its nodes' counters in order.
struct Transport {
    client: Arc<dyn Client>,
    statistics: Vec<Statistics>,
}

impl Swap {
    /// Calls reach `transport`; `statistics` are its nodes' counters in
    /// configured order.
    #[must_use]
    pub fn new(transport: Arc<dyn Client>, statistics: Vec<Statistics>) -> Self {
        Self(Mutex::new(Transport {
            client: transport,
            statistics,
        }))
    }

    /// Every call that starts after this returns reaches `transport`, and
    /// [`Self::statistics`] answers with `statistics`.
    pub fn replace(&self, transport: Arc<dyn Client>, statistics: Vec<Statistics>) {
        *self.installed() = Transport {
            client: transport,
            statistics,
        };
    }

    /// The counters of the nodes calls reach now, in configured order.
    #[must_use]
    pub fn statistics(&self) -> Vec<Statistics> {
        self.installed().statistics.clone()
    }

    fn current(&self) -> Arc<dyn Client> {
        self.installed().client.clone()
    }

    // Each critical section is one assignment or one clone, so a panic cannot
    // leave the pair torn: recovering the guard is sound.
    fn installed(&self) -> std::sync::MutexGuard<'_, Transport> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for Swap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Swap")
            .field("node_count", &self.installed().statistics.len())
            .finish()
    }
}

impl Client for Swap {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<CallResult, Error>> {
        let transport = self.current();
        Box::pin(async move { transport.request(request).await })
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
        let transport = self.current();
        Box::pin(async move { transport.batch(batch).await })
    }

    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), CallError>> {
        let transport = self.current();
        Box::pin(async move { transport.broadcast(request).await })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testing::{Reply, Script};

    #[tokio::test]
    async fn a_replacement_redirects_existing_holders() {
        let old = Script::new([Reply::ok("m", json!(1))]);
        let swap = Arc::new(Swap::new(Arc::new(old.clone()), Vec::new()));
        let holder: Arc<dyn Client> = swap.clone();
        let reply = holder.request(Request::new("m", json!([]))).await;
        assert_eq!(reply.unwrap().unwrap().deserialize::<u64>().unwrap(), 1);

        let next = Script::new([Reply::ok("m", json!(2))]);
        swap.replace(Arc::new(next.clone()), Vec::new());
        let reply = holder.request(Request::new("m", json!([]))).await;
        assert_eq!(reply.unwrap().unwrap().deserialize::<u64>().unwrap(), 2);
        old.exhausted();
        next.exhausted();
    }
}
