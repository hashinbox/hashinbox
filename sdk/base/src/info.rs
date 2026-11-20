//! [`InfoClient`]: a chain's client over a node, telling its tip and chain;
//! [`NodeInfo`], what it tells; and [`CachedInfo`], its tip asked at most
//! once per block time.

use std::{sync::Arc, time::Duration};

use crate::{Cache, Future};

/// A chain's client over one node: its tip and what it serves.
pub trait InfoClient: rpc::Client {
    /// The chain's reader of `node`; asks it nothing.
    fn new(node: Arc<dyn rpc::Client>) -> Self
    where
        Self: Sized;

    /// The node's newest block (or slot).
    fn tip(&self) -> Future<'_, anyhow::Result<u64>>;

    /// Its chain and tip, as the node reports them; judging them is the
    /// caller's.
    ///
    /// # Errors
    ///
    /// A node failure.
    fn info(&self) -> Future<'_, anyhow::Result<NodeInfo>>;
}

/// What a node says about itself: which chain it serves and its tip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeInfo {
    /// The chain it serves, as the network's `chain_id` writes it (EVM chain
    /// ID, Solana genesis hash), or its name where that is enough (Bitcoin).
    pub chain_id: String,
    /// Its newest block (or slot).
    pub tip: u64,
}

/// An [`InfoClient`] whose tip is asked at most once per `ttl` (the
/// network's block time); between asks, the last answer.
pub struct CachedInfo {
    inner: Arc<dyn InfoClient>,
    tip: Cache<u64>,
}

impl CachedInfo {
    /// `inner`, its tip kept for `ttl`; asks nothing.
    #[must_use]
    pub fn new(inner: Arc<dyn InfoClient>, ttl: Duration) -> Self {
        Self {
            inner,
            tip: Cache::new(ttl),
        }
    }

    /// The tip: the kept one while fresh, else asked.
    ///
    /// # Errors
    ///
    /// The node's failure; nothing is kept.
    pub async fn tip(&self) -> anyhow::Result<u64> {
        self.tip.get(|| self.inner.tip()).await
    }

    /// The last tip kept, however old; `None` before the first. Asks nothing.
    pub async fn last(&self) -> Option<u64> {
        self.tip.last().await
    }

    /// Its chain and tip, asked now.
    ///
    /// # Errors
    ///
    /// The node's failure.
    pub async fn info(&self) -> anyhow::Result<NodeInfo> {
        self.inner.info().await
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use rpc::testing::{Reply, Script};
    use serde_json::json;

    use super::{CachedInfo, InfoClient};
    use crate::testing::TipInfo;

    #[tokio::test]
    async fn the_tip_is_asked_at_most_once_per_ttl() {
        let script = Script::new([Reply::ok("tip", json!(5))]);
        let node: Arc<dyn InfoClient> = Arc::new(TipInfo::new(Arc::new(script.clone())));
        let cached = CachedInfo::new(node.clone(), Duration::from_secs(60));
        assert_eq!(cached.last().await, None);
        for _ in 0..3 {
            assert_eq!(cached.tip().await.unwrap(), 5);
        }
        assert_eq!(cached.last().await, Some(5));
        script.exhausted();

        let stale = CachedInfo::new(node, Duration::ZERO);
        script.push([Reply::ok("tip", json!(6)), Reply::ok("tip", json!(7))]);
        assert_eq!(stale.tip().await.unwrap(), 6);
        assert_eq!(stale.tip().await.unwrap(), 7);
        script.exhausted();
    }
}
