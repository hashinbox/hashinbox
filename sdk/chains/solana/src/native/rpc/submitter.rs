//! Submission: the chain's only external effect, and the reads that judge it.

use base::BoxFuture;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Client, Reading};
use crate::native::{Error, SignedTransaction, TransactionId};

/// Transaction submission and the reads that settle its outcome.
pub trait Submitter: Send + Sync {
    /// Submits the exact signed bytes once, with node preflight and no node
    /// rebroadcast, and checks the node returns the local signature.
    ///
    /// Every failure is [`crate::native::ErrorKind::RpcUnavailable`]: the
    /// bytes may have reached the node, so the outcome is unknown and only
    /// `transaction.id` identifies it.
    fn broadcast<'a>(
        &'a self,
        transaction: &'a SignedTransaction,
        min_slot: u64,
    ) -> BoxFuture<'a, Result<(), Error>>;

    /// Whether the node has seen `transaction` land, successfully or not, at
    /// or after the slot `min_slot`.
    fn known<'a>(
        &'a self,
        transaction: &'a TransactionId,
        min_slot: u64,
    ) -> BoxFuture<'a, Result<bool, Error>>;

    /// The confirmed block height, read at or after the slot `min_slot`.
    fn height(&self, min_slot: u64) -> BoxFuture<'_, Result<u64, Error>>;
}

/// Where a transaction landed, as `getSignatureStatuses` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignatureStatus {
    /// The slot that processed it.
    pub slot: u64,
    /// Whether execution failed; a failed transaction still paid its fee.
    pub failed: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusWire {
    slot: u64,
    confirmations: Option<u64>,
    err: Option<Value>,
    confirmation_status: String,
}

impl<C: json_rpc::Client> Submitter for Client<C> {
    fn broadcast<'a>(
        &'a self,
        transaction: &'a SignedTransaction,
        min_slot: u64,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let unknown = || Error::rpc("Solana submission outcome is unknown", true);
            let returned = self
                .request::<String>(
                    "sendTransaction",
                    json!([STANDARD.encode(transaction.envelope.as_bytes()), {
                        "encoding":"base64",
                        "skipPreflight":false,
                        "preflightCommitment":"confirmed",
                        "minContextSlot":min_slot,
                        "maxRetries":0
                    }]),
                )
                .await
                .map_err(|_| unknown())?;
            if returned.parse::<TransactionId>().ok() != Some(transaction.id) {
                return Err(unknown());
            }
            Ok(())
        })
    }

    fn known<'a>(
        &'a self,
        transaction: &'a TransactionId,
        min_slot: u64,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move {
            Ok(self
                .signature_status(transaction, min_slot)
                .await?
                .value
                .is_some())
        })
    }

    fn height(&self, min_slot: u64) -> BoxFuture<'_, Result<u64, Error>> {
        Box::pin(async move {
            self.request(
                "getBlockHeight",
                json!([{"commitment":"confirmed", "minContextSlot":min_slot}]),
            )
            .await
        })
    }
}

impl<C: json_rpc::Client> Client<C> {
    /// Where `transaction` landed, searching the node's whole history; `None`
    /// when the node has not seen it.
    ///
    /// # Errors
    ///
    /// [`crate::native::ErrorKind::RpcUnavailable`], also for a status that
    /// contradicts itself or lies outside `min_slot..=` the reply's slot.
    pub async fn signature_status(
        &self,
        transaction: &TransactionId,
        min_slot: u64,
    ) -> Result<Reading<Option<SignatureStatus>>, Error> {
        let method = "getSignatureStatuses";
        let context = self
            .contextual::<Vec<Option<StatusWire>>>(
                method,
                json!([[transaction.to_string()], {"searchTransactionHistory":true}]),
                min_slot,
            )
            .await?;
        let [status] = <[Option<StatusWire>; 1]>::try_from(context.value)
            .map_err(|_| Error::malformed(method))?;
        let status = status
            .map(|status| {
                status
                    .coherent(min_slot, context.slot)
                    .ok_or_else(|| Error::malformed(method))
            })
            .transpose()?;
        Ok(Reading {
            slot: context.slot,
            value: status,
        })
    }
}

impl StatusWire {
    /// The status, unless it lies outside `min_slot..=slot`, names an unknown
    /// commitment, or claims confirmations after finalization.
    fn coherent(self, min_slot: u64, slot: u64) -> Option<SignatureStatus> {
        let known = matches!(
            self.confirmation_status.as_str(),
            "processed" | "confirmed" | "finalized"
        );
        let finalized = self.confirmation_status == "finalized";
        if self.slot < min_slot
            || self.slot > slot
            || !known
            || finalized && self.confirmations.is_some()
        {
            return None;
        }
        Some(SignatureStatus {
            slot: self.slot,
            failed: self.err.is_some(),
        })
    }
}

#[cfg(test)]
mod tests {
    use solana_signature::Signature;

    use super::*;
    use crate::native::{
        Address,
        rpc::script::{Script, Scripted},
        transaction::Lifetime,
    };

    fn signed(signature: u8, bytes: Vec<u8>) -> SignedTransaction {
        SignedTransaction {
            id: TransactionId::from(Signature::from([signature; 64])),
            envelope: base::transaction::Envelope::new(bytes),
            from: Address::from_bytes([1; 32]),
            min_slot: 11,
            lifetime: Lifetime::new(solana_hash::Hash::new_from_array([9; 32]), 44),
        }
    }

    fn send(bytes: &[u8], returned: Value) -> Script {
        Script::exchanges([(
            "sendTransaction",
            json!([STANDARD.encode(bytes), {
                "encoding": "base64",
                "skipPreflight": false,
                "preflightCommitment": "confirmed",
                "minContextSlot": 11,
                "maxRetries": 0,
            }]),
            returned,
        )])
    }

    #[tokio::test]
    async fn sends_exact_bytes_and_accepts_only_the_local_signature() {
        let transaction = signed(7, vec![8, 9]);
        let rpc = send(&[8, 9], json!(transaction.id.to_string()));
        rpc.client()
            .broadcast(&transaction, 11)
            .await
            .expect("matching signature");
        rpc.exhausted();

        for returned in [
            json!(Signature::from([8; 64]).to_string()),
            json!("malformed"),
            json!(""),
            json!(7),
        ] {
            let rpc = send(&[8, 9], returned);
            let error = rpc
                .client()
                .broadcast(&transaction, 11)
                .await
                .expect_err("a mismatched or malformed response stays ambiguous");
            assert_eq!(
                error,
                Error::rpc("Solana submission outcome is unknown", true)
            );
            rpc.exhausted();
        }
    }

    #[tokio::test]
    async fn reads_height_and_exactly_one_coherent_historical_status() {
        let id = TransactionId::from(Signature::from([7; 64]));
        let params = json!([[id.to_string()], {"searchTransactionHistory":true}]);
        let rpc = Script::exchanges([
            (
                "getSignatureStatuses",
                params.clone(),
                json!({"context":{"slot":15},"value":[null]}),
            ),
            (
                "getSignatureStatuses",
                params.clone(),
                json!({
                    "context": {"slot": 15},
                    "value": [{
                        "slot": 12,
                        "confirmations": null,
                        "err": {"x": 1},
                        "confirmationStatus": "finalized",
                    }],
                }),
            ),
            (
                "getSignatureStatuses",
                params,
                json!({"context":{"slot":15},"value":[{
                    "slot": 12, "confirmations": 1, "err": null, "confirmationStatus": "confirmed",
                }]}),
            ),
            (
                "getBlockHeight",
                json!([{"commitment":"confirmed", "minContextSlot":10}]),
                json!(99),
            ),
        ]);
        let client = rpc.client();
        assert!(!client.known(&id, 10).await.unwrap());
        let status = client
            .signature_status(&id, 10)
            .await
            .unwrap()
            .value
            .unwrap();
        assert_eq!(
            status,
            SignatureStatus {
                slot: 12,
                failed: true
            }
        );
        assert!(client.known(&id, 10).await.unwrap());
        assert_eq!(client.height(10).await.unwrap(), 99);
        rpc.exhausted();
    }

    #[tokio::test]
    async fn status_context_floor_precedes_its_cardinality() {
        let id = TransactionId::from(Signature::from([7; 64]));
        for (slot, expected) in [
            (3, Error::below_min_slot()),
            (4, Error::malformed("getSignatureStatuses")),
        ] {
            let rpc = Script::exchanges([(
                "getSignatureStatuses",
                json!([[id.to_string()], {"searchTransactionHistory":true}]),
                json!({"context":{"slot":slot},"value":[]}),
            )]);
            assert_eq!(rpc.client().known(&id, 4).await.unwrap_err(), expected);
            rpc.exhausted();
        }
    }

    #[tokio::test]
    async fn malformed_historical_status_preserves_its_method_context() {
        let id = TransactionId::from(Signature::from([7; 64]));
        for value in [
            json!([]),
            json!([null, null]),
            json!([{"slot":9,"confirmations":null,"err":null,"confirmationStatus":"finalized"}]),
            json!([{"slot":16,"confirmations":null,"err":null,"confirmationStatus":"finalized"}]),
            json!([{"slot":12,"confirmations":1,"err":null,"confirmationStatus":"finalized"}]),
            json!([{"slot":12,"confirmations":null,"err":null,"confirmationStatus":"unknown"}]),
        ] {
            let rpc = Script::exchanges([(
                "getSignatureStatuses",
                json!([[id.to_string()], {"searchTransactionHistory":true}]),
                json!({"context":{"slot":15},"value":value}),
            )]);
            let error = rpc.client().signature_status(&id, 10).await.unwrap_err();
            assert_eq!(error, Error::malformed("getSignatureStatuses"));
            rpc.exhausted();
        }
    }
}
