//! Scripted answers: [`Held`] balances, [`Confirms`] verdicts, [`Holders`]
//! and [`TipInfo`], a node's tip.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, PoisonError},
};

use anyhow::Context as _;
use serde_json::json;

use super::lock;
use crate::{
    Address, Asset, Balances, Confirmed, Confirmer, Decimal, Future, Holding, Holdings, InfoClient,
    Network, Networked, NodeInfo, Spends, TransactionId,
};

/// What the chain says each address holds, by its text: a holding,
/// answered whatever final block was asked for, or `None` (not answerable
/// yet); a read of an address not set fails.
pub struct Held {
    network: Network,
    held: Mutex<BTreeMap<String, Option<Holding>>>,
}

impl Held {
    /// Balances of `network` that know of no address yet.
    #[must_use]
    pub fn new(network: Network) -> Self {
        Self {
            network,
            held: Mutex::default(),
        }
    }

    /// `address` holds `amount` at `position`; `None`: not answerable.
    pub fn set(&self, address: &str, held: Option<(u64, u64)>) {
        let held = held.map(|(position, amount)| Holding {
            position,
            amount: Decimal::from(amount),
        });
        lock(&self.held).insert(address.to_owned(), held);
    }
}

impl Networked for Held {
    fn network(&self) -> &Network {
        &self.network
    }
}

impl Balances for Held {
    fn balance<'a>(
        &'a self,
        address: &'a Address,
        _asset: &'a Asset,
        _at: u64,
    ) -> Future<'a, anyhow::Result<Option<Holding>>> {
        let held = lock(&self.held).get(&address.value).cloned();
        Box::pin(async move { held.context("the node cannot read this balance") })
    }
}

/// What the chain says of each transaction, by its txid: judged at a
/// block, found in one (reverted there or not) or not, or `None` (not
/// answerable yet); a read of a txid not set fails.
pub struct Confirms {
    network: Network,
    judged: Mutex<BTreeMap<String, Option<Confirmed>>>,
}

impl Confirms {
    /// A confirmer of `network` that knows of no transaction yet.
    #[must_use]
    pub fn new(network: Network) -> Self {
        Self {
            network,
            judged: Mutex::default(),
        }
    }

    /// `txid` is judged at `position`, `found` in a block or not, `reverted`
    /// there or not; `None`: not answerable.
    pub fn set(&self, txid: &str, judged: Option<(u64, Option<u64>, bool)>) {
        let judged = judged.map(|(position, found, reverted)| Confirmed {
            position,
            found,
            reverted,
        });
        lock(&self.judged).insert(txid.to_owned(), judged);
    }
}

impl Networked for Confirms {
    fn network(&self) -> &Network {
        &self.network
    }
}

impl Confirmer for Confirms {
    fn confirm<'a>(
        &'a self,
        id: &'a TransactionId,
        _at: u64,
    ) -> Future<'a, anyhow::Result<Option<Confirmed>>> {
        let judged = lock(&self.judged).get(&id.id).cloned();
        Box::pin(async move { judged.context("the node cannot tell") })
    }
}

/// [`Holdings`] where every recipient holds every asset ([`Holders::all`])
/// or only the addresses named ([`Holders::of`]), until told otherwise
/// ([`Holders::hold_as`]).
pub struct Holders(Mutex<Option<Vec<String>>>);

impl Holders {
    /// Every recipient holds every asset.
    #[must_use]
    pub const fn all() -> Self {
        Self(Mutex::new(None))
    }

    /// The addresses whose text is one of `addresses` hold every asset.
    #[must_use]
    pub fn of<'a>(addresses: impl IntoIterator<Item = &'a str>) -> Self {
        let of = addresses.into_iter().map(str::to_owned).collect();
        Self(Mutex::new(Some(of)))
    }

    /// From now on, hold as `holders` does.
    pub fn hold_as(&self, holders: Self) {
        let holders = holders
            .0
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        *lock(&self.0) = holders;
    }
}

impl Holdings for Holders {
    fn holds<'a>(&'a self, spends: &'a Spends) -> Future<'a, anyhow::Result<Vec<bool>>> {
        let of = lock(&self.0).clone();
        let holds = |to: &Address| of.as_ref().is_none_or(|of| of.contains(&to.value));
        let held = spends
            .iter()
            .map(|spend| spend.to.as_ref().is_some_and(holds))
            .collect();
        Box::pin(async move { Ok(held) })
    }
}

/// Forwards every call of `rpc::Client` to the node `.0`.
macro_rules! forward {
    ($($info:ty),+) => {$(
        impl rpc::Client for $info {
            fn request<'a>(
                &'a self,
                request: rpc::Request,
            ) -> rpc::BoxFuture<'a, Result<rpc::RawJson, rpc::Error>> {
                self.0.request(request)
            }

            fn batch<'a>(
                &'a self,
                batch: rpc::Batch,
            ) -> rpc::BoxFuture<'a, Result<Vec<rpc::Answer>, rpc::Error>> {
                self.0.batch(batch)
            }

            fn broadcast<'a>(
                &'a self,
                request: rpc::Request,
            ) -> rpc::BoxFuture<'a, Result<(), rpc::Error>> {
                self.0.broadcast(request)
            }
        }
    )+};
}

forward!(TipInfo, Unasked);

/// An [`InfoClient`] over an rpc client: its tip is the node's `tip` call;
/// it tells no info.
pub struct TipInfo(Arc<dyn rpc::Client>);

/// An [`InfoClient`] that asks its node nothing: at tip 0, serving chain
/// [`Unasked::CHAIN_ID`]. Calls through it reach the node.
pub struct Unasked(Arc<dyn rpc::Client>);

impl Unasked {
    /// The chain it says its node serves.
    pub const CHAIN_ID: &str = "unasked";
}

impl InfoClient for Unasked {
    fn new(node: Arc<dyn rpc::Client>) -> Self {
        Self(node)
    }

    fn tip(&self) -> Future<'_, anyhow::Result<u64>> {
        Box::pin(async { Ok(0) })
    }

    fn info(&self) -> Future<'_, anyhow::Result<NodeInfo>> {
        let info = NodeInfo {
            chain_id: Self::CHAIN_ID.to_owned(),
            tip: 0,
        };
        Box::pin(async move { Ok(info) })
    }
}

impl InfoClient for TipInfo {
    fn new(node: Arc<dyn rpc::Client>) -> Self {
        Self(node)
    }

    fn tip(&self) -> Future<'_, anyhow::Result<u64>> {
        Box::pin(async move {
            let raw = self.0.request(rpc::Request::new("tip", json!([]))).await?;
            Ok(raw.deserialize()?)
        })
    }

    fn info(&self) -> Future<'_, anyhow::Result<NodeInfo>> {
        Box::pin(async { anyhow::bail!("TipInfo tells only its tip") })
    }
}
