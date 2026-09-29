//! Shapes shared by contextual Solana RPC replies.

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use super::Client;
use crate::native::Error;

/// How settled the state a read observes must be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Commitment {
    /// Voted on by a supermajority; may still roll back.
    Confirmed,
    /// Rooted: cannot roll back.
    Finalized,
}

impl Commitment {
    pub(super) const fn text(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Finalized => "finalized",
        }
    }

    /// `{"commitment": …}` plus `minContextSlot` when a minimum is required.
    pub(super) fn config(self, minimum: Option<u64>) -> Value {
        match minimum {
            Some(min_slot) => json!({ "commitment": self.text(), "minContextSlot": min_slot }),
            None => json!({ "commitment": self.text() }),
        }
    }
}

/// A value read at the node slot `slot`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading<T> {
    /// The slot the node read `value` at; later reads use it as their minimum.
    pub slot: u64,
    /// The value read.
    pub value: T,
}

#[derive(Deserialize)]
pub(super) struct ContextWire<T> {
    pub(super) context: SlotWire,
    pub(super) value: T,
}

#[derive(Deserialize)]
pub(super) struct SlotWire {
    pub(super) slot: u64,
}

impl<T> ContextWire<T> {
    /// Rejects a reply read before `min_slot`: it may predate state the caller
    /// already observed.
    pub(super) fn require_min_slot(&self, min_slot: Option<u64>) -> Result<(), Error> {
        if min_slot.is_some_and(|min_slot| self.context.slot < min_slot) {
            return Err(Error::below_min_slot());
        }
        Ok(())
    }

    /// The value with its slot, once the minimum holds.
    pub(super) fn at_least(self, min_slot: Option<u64>) -> Result<Reading<T>, Error> {
        self.require_min_slot(min_slot)?;
        Ok(Reading {
            slot: self.context.slot,
            value: self.value,
        })
    }
}

impl<C: json_rpc::Client> Client<C> {
    /// One contextual read of `method` at or after the slot `min_slot`.
    pub(crate) async fn contextual<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
        min_slot: u64,
    ) -> Result<Reading<T>, Error> {
        self.request::<ContextWire<T>>(method, params)
            .await?
            .at_least(Some(min_slot))
    }
}
