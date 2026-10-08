use std::str::FromStr;

use base::BoxFuture;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use solana_hash::Hash;

use super::Lifetime;
use crate::native::{
    Error, ErrorKind, Lamport,
    rpc::{Client, Reading},
};

/// The node context one transfer is built and checked against. Each read
/// is at or after the slot `min_slot` of the previous one.
pub trait Preflight: Send + Sync {
    /// A recent confirmed blockhash and its expiry height.
    fn lifetime(&self, min_slot: u64) -> BoxFuture<'_, Result<Reading<Lifetime>, Error>>;

    /// The fee of the exact serialized `message`.
    fn fee<'a>(
        &'a self,
        message: &'a [u8],
        min_slot: u64,
    ) -> BoxFuture<'a, Result<Reading<Lamport>, Error>>;

    /// Simulates the exact signed `transaction` with signature verification
    /// and returns the slot it ran at.
    ///
    /// A failed simulation is [`ErrorKind::Rejected`].
    fn simulate<'a>(
        &'a self,
        transaction: &'a [u8],
        min_slot: u64,
    ) -> BoxFuture<'a, Result<u64, Error>>;
}

/// A lifetime as the node spells it. The blockhash stays text until the
/// context floor holds and the text proves canonical, so a stale reply is
/// refused as stale before it is refused as malformed, and [`Lifetime`]
/// never learns the node's encoding.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LifetimeWire {
    blockhash: String,
    last_valid_block_height: u64,
}

#[derive(Deserialize)]
struct SimulationWire {
    err: Option<Value>,
}

impl<C: json_rpc::Client> Preflight for Client<C> {
    fn lifetime(&self, min_slot: u64) -> BoxFuture<'_, Result<Reading<Lifetime>, Error>> {
        Box::pin(async move {
            let method = "getLatestBlockhash";
            let context = self
                .contextual::<LifetimeWire>(
                    method,
                    json!([{"commitment":"confirmed", "minContextSlot":min_slot}]),
                    min_slot,
                )
                .await?;
            let text = &context.value.blockhash;
            let blockhash = Hash::from_str(text).map_err(|_| Error::malformed(method))?;
            if blockhash.to_string() != *text {
                return Err(Error::malformed(method));
            }
            Ok(Reading {
                slot: context.slot,
                value: Lifetime::new(blockhash, context.value.last_valid_block_height),
            })
        })
    }

    fn fee<'a>(
        &'a self,
        message: &'a [u8],
        min_slot: u64,
    ) -> BoxFuture<'a, Result<Reading<Lamport>, Error>> {
        Box::pin(async move {
            let context = self
                .contextual::<Option<u64>>(
                    "getFeeForMessage",
                    json!([STANDARD.encode(message), {
                        "commitment": "confirmed",
                        "minContextSlot": min_slot,
                    }]),
                    min_slot,
                )
                .await?;
            let fee = context.value.ok_or_else(|| {
                Error::new(ErrorKind::FeeUnavailable, "Solana fee is unavailable")
            })?;
            Ok(Reading {
                slot: context.slot,
                value: Lamport::from_atomic(fee),
            })
        })
    }

    fn simulate<'a>(
        &'a self,
        transaction: &'a [u8],
        min_slot: u64,
    ) -> BoxFuture<'a, Result<u64, Error>> {
        Box::pin(async move {
            let context = self
                .contextual::<SimulationWire>(
                    "simulateTransaction",
                    json!([STANDARD.encode(transaction), {
                        "encoding":"base64",
                        "commitment":"confirmed",
                        "sigVerify":true,
                        "replaceRecentBlockhash":false,
                        "minContextSlot":min_slot
                    }]),
                    min_slot,
                )
                .await?;
            if context.value.err.is_some() {
                return Err(Error::new(
                    ErrorKind::Rejected,
                    "Solana transaction simulation failed",
                ));
            }
            Ok(context.slot)
        })
    }
}
