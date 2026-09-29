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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::native::rpc::script::{Script, Scripted};

    fn simulation(
        transaction: &[u8],
        min_slot: u64,
        result: Value,
    ) -> (&'static str, Value, Value) {
        (
            "simulateTransaction",
            json!([STANDARD.encode(transaction), {
                "encoding": "base64",
                "commitment": "confirmed",
                "sigVerify": true,
                "replaceRecentBlockhash": false,
                "minContextSlot": min_slot,
            }]),
            result,
        )
    }

    #[tokio::test]
    async fn context_floor_precedes_fee_and_simulation_semantics() {
        for slot in [3, 4] {
            let rpc = Script::exchanges([
                (
                    "getFeeForMessage",
                    json!([STANDARD.encode([]), {"commitment":"confirmed", "minContextSlot":4}]),
                    json!({"context":{"slot":slot},"value":null}),
                ),
                simulation(
                    &[],
                    4,
                    json!({"context":{"slot":slot},"value":{"err":{"x":1}}}),
                ),
            ]);
            let client = rpc.client();
            let errors = [
                client.fee(&[], 4).await.unwrap_err(),
                client.simulate(&[], 4).await.unwrap_err(),
            ];
            let expected = if slot < 4 {
                [Error::below_min_slot(), Error::below_min_slot()]
            } else {
                [
                    Error::new(ErrorKind::FeeUnavailable, "Solana fee is unavailable"),
                    Error::new(ErrorKind::Rejected, "Solana transaction simulation failed"),
                ]
            };
            assert_eq!(errors, expected);
            rpc.exhausted();
        }
    }

    #[tokio::test]
    async fn reads_lifetime_fee_and_simulation_with_exact_floors() {
        let blockhash = Hash::new_from_array([9; 32]).to_string();
        let message = [1, 2, 3];
        let transaction = [4, 5, 6];
        let rpc = Script::exchanges([
            (
                "getLatestBlockhash",
                json!([{"commitment":"confirmed", "minContextSlot":7}]),
                json!({
                    "context": {"slot": 8},
                    "value": {"blockhash": blockhash, "lastValidBlockHeight": 99},
                }),
            ),
            (
                "getFeeForMessage",
                json!([STANDARD.encode(message), {"commitment":"confirmed", "minContextSlot":8}]),
                json!({"context":{"slot":9},"value":5000}),
            ),
            simulation(
                &transaction,
                9,
                json!({"context":{"slot":10},"value":{"err":null,"logs":[]}}),
            ),
        ]);
        let client = rpc.client();
        let lifetime = client.lifetime(7).await.expect("lifetime");
        assert_eq!(lifetime.slot, 8);
        assert_eq!(lifetime.value.last_valid_block_height(), 99);
        assert_eq!(client.fee(&message, 8).await.unwrap().value.units(), 5000);
        assert_eq!(client.simulate(&transaction, 9).await.unwrap(), 10);
        rpc.exhausted();
    }

    #[tokio::test]
    async fn lifetime_content_errors_keep_method_context_after_floor_validation() {
        for (slot, expected) in [
            (3, Error::below_min_slot()),
            (4, Error::malformed("getLatestBlockhash")),
        ] {
            let rpc = Script::exchanges([(
                "getLatestBlockhash",
                json!([{"commitment":"confirmed", "minContextSlot":4}]),
                json!({
                    "context": {"slot": slot},
                    "value": {"blockhash": "bad", "lastValidBlockHeight": 9},
                }),
            )]);
            assert_eq!(rpc.client().lifetime(4).await.unwrap_err(), expected);
            rpc.exhausted();
        }
    }
}
