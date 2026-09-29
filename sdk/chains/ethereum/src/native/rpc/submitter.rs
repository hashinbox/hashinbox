//! Submission: the chain's only external effect, and the read that judges it.
//!
//! `Submitter::broadcast` sends one exact envelope with one visible attempt
//! and verifies the hash the node returns against the locally computed one;
//! `known` asks whether the node holds that exact hash, which is the only way
//! to settle an attempt whose outcome the node never reported.

use alloy_primitives::{hex, keccak256};
use base::BoxFuture;
use json_rpc::CallError;
use serde_json::{Value, json};

use super::{Client, Remote};
use crate::native::{Error, ErrorKind, SignedTransaction, TransactionId};

/// Transaction submission: the chain's only external effect.
pub trait Submitter: Send + Sync {
    /// Submits one exact signed envelope and verifies the returned hash.
    ///
    /// Implementations make one visible submission attempt. Any
    /// [`ErrorKind::RpcUnavailable`] means the attempt may have reached the
    /// node: the outcome is unknown and only `transaction.id` identifies it.
    /// Every other kind is a definite failure before the wire.
    fn broadcast<'a>(
        &'a self,
        transaction: SignedTransaction,
    ) -> BoxFuture<'a, Result<TransactionId, Error>>;

    /// Whether the node exposes the exact transaction hash.
    fn known<'a>(&'a self, transaction: &'a TransactionId) -> BoxFuture<'a, Result<bool, Error>>;
}

impl<C: json_rpc::Client> Submitter for Client<C> {
    fn broadcast<'a>(
        &'a self,
        transaction: SignedTransaction,
    ) -> BoxFuture<'a, Result<TransactionId, Error>> {
        Box::pin(async move {
            let bytes = transaction.envelope.as_bytes();
            let computed = TransactionId::from_bytes(keccak256(bytes).0);
            if computed != transaction.id {
                return Err(Error::new(
                    ErrorKind::InvalidTransaction,
                    "signed Ethereum envelope hash does not match its transaction ID",
                ));
            }
            let method = "eth_sendRawTransaction";
            let raw = match self
                .call_once(method, json!([hex::encode_prefixed(bytes)]))
                .await
            {
                Ok(raw) => raw,
                Err(error) if error.is_already_known() => {
                    return self.confirmed_submission(computed).await;
                }
                Err(error @ CallError::Remote(_)) => {
                    return Err(ambiguous(Error::from_call(method, error)));
                }
                Err(CallError::Transport(error)) => {
                    return Err(ambiguous(Error::from(error)));
                }
            };
            let returned: String = raw
                .deserialize()
                .map_err(|error| ambiguous(Error::from(error)))?;
            let returned = TransactionId::from_rpc(&returned, method).map_err(ambiguous)?;
            if returned != computed {
                return Err(ambiguous(
                    "Ethereum submission response hash differs from the exact signed \
                     envelope; outcome is ambiguous",
                ));
            }
            Ok(returned)
        })
    }

    fn known<'a>(&'a self, transaction: &'a TransactionId) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move {
            let method = "eth_getTransactionByHash";
            let value: Value = self
                .request(method, json!([transaction.to_string()]))
                .await?
                .deserialize()?;
            if value.is_null() {
                return Ok(false);
            }
            let returned = value
                .get("hash")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::invalid_response(method, "transaction object has no hash"))?;
            if &TransactionId::from_rpc(returned, method)? != transaction {
                return Err(Error::invalid_response(
                    method,
                    "transaction object hash does not match the lookup",
                ));
            }
            Ok(true)
        })
    }
}

impl<C: json_rpc::Client> Client<C> {
    async fn confirmed_submission(&self, computed: TransactionId) -> Result<TransactionId, Error> {
        match self.known(&computed).await {
            Ok(true) => Ok(computed),
            Ok(false) => Err(ambiguous(
                "Ethereum RPC reported an already-known transaction but did \
                 not expose the matching hash",
            )),
            Err(error) => Err(ambiguous(error)),
        }
    }
}

/// An unknown submission outcome: the caller holds the exact local ID.
fn ambiguous(error: impl std::fmt::Display) -> Error {
    Error::rpc(
        format!("Ethereum submission outcome is ambiguous: {error}"),
        true,
    )
}

#[cfg(test)]
mod tests {
    use base::transaction::Envelope;
    use futures_executor::block_on;

    use super::*;
    use crate::native::rpc::script::{Reply, Script, Scripted};

    fn signed(bytes: &[u8]) -> SignedTransaction {
        SignedTransaction {
            id: TransactionId::from_bytes(keccak256(bytes).0),
            envelope: Envelope::new(bytes.to_vec()),
        }
    }

    #[test]
    fn exact_envelope_broadcast_rejects_a_mismatched_provider_hash() {
        let transaction = signed(&[0x02, 0x01, 0x02, 0x03]);
        let provider_candidate = format!("0x{}", "dd".repeat(32));
        let script = Script::node(vec![
            Reply::ok("eth_sendRawTransaction", json!(provider_candidate.clone()))
                .expecting(json!([hex::encode_prefixed([0x02, 0x01, 0x02, 0x03])])),
        ]);
        let error = block_on(script.client().broadcast(transaction.clone()))
            .expect_err("provider hash mismatch must fail");
        assert_eq!(error.kind, ErrorKind::RpcUnavailable);
        assert!(!error.message.contains(&provider_candidate));
    }

    #[test]
    fn mismatched_local_id_is_rejected_before_submission_without_ambiguity() {
        let script = Script::node(Vec::new());
        let mut transaction = signed(&[0x02, 0x10, 0x20, 0x30]);
        transaction.id = TransactionId::from_bytes([0x99; 32]);
        let error = block_on(script.client().broadcast(transaction)).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidTransaction);
        assert_eq!(
            error.message,
            "signed Ethereum envelope hash does not match its transaction ID"
        );
        assert!(script.requests().is_empty());
    }

    #[test]
    fn unknown_remote_submission_failure_remains_ambiguous() {
        let transaction = signed(&[0x02, 0x04, 0x05, 0x06]);
        let script = Script::node(vec![Reply::failure(
            "eth_sendRawTransaction",
            -32_000,
            "backend failed after upstream acceptance: Bearer secret",
        )]);
        let error = block_on(script.client().broadcast(transaction.clone())).unwrap_err();
        assert_eq!(error.kind, ErrorKind::RpcUnavailable);
        assert_eq!(
            error.message,
            "Ethereum submission outcome is ambiguous: Ethereum JSON-RPC \
             eth_sendRawTransaction failed with code -32000"
        );
    }

    #[test]
    fn missing_and_malformed_submission_results_keep_the_exact_local_id() {
        for result in [Value::Null, json!(7)] {
            let transaction = signed(&[0x02, 0x07, 0x08, 0x09]);
            let script = Script::node(vec![Reply::ok("eth_sendRawTransaction", result)]);
            let error = block_on(script.client().broadcast(transaction.clone())).unwrap_err();
            assert_eq!(error.kind, ErrorKind::RpcUnavailable);
        }
    }

    #[test]
    fn already_known_succeeds_only_after_matching_hash_lookup() {
        let transaction = signed(&[0x02, 0xaa, 0xbb]);
        let id = transaction.id.to_string();
        for (visible, known) in [
            (true, json!({"hash": id})),
            (false, json!({"hash": format!("0x{}", "ee".repeat(32))})),
            (false, Value::Null),
        ] {
            let script = Script::node(vec![
                Reply::failure("eth_sendRawTransaction", -32_000, "already known")
                    .expecting(json!([hex::encode_prefixed([0x02, 0xaa, 0xbb])])),
                Reply::ok("eth_getTransactionByHash", known).expecting(json!([id])),
            ]);
            let result = block_on(script.client().broadcast(transaction.clone()));
            if visible {
                assert_eq!(result.unwrap(), transaction.id);
            } else {
                assert_eq!(result.unwrap_err().kind, ErrorKind::RpcUnavailable);
            }
            script.exhausted();
        }
        let script = Script::node(vec![
            Reply::failure("eth_sendRawTransaction", -32_000, "already known"),
            Reply::ok("eth_getTransactionByHash", Value::Null),
        ]);
        assert_eq!(
            block_on(script.client().broadcast(transaction))
                .unwrap_err()
                .message,
            "Ethereum submission outcome is ambiguous: Ethereum RPC reported an \
             already-known transaction but did not expose the matching hash"
        );
    }

    #[test]
    fn known_requires_the_exact_transaction_hash() {
        let id = TransactionId::from_bytes([0xaa; 32]);
        let absent = Script::node(vec![Reply::ok("eth_getTransactionByHash", Value::Null)]);
        assert!(!block_on(absent.client().known(&id)).expect("null lookup means not known"));
        let matching = Script::node(vec![Reply::ok(
            "eth_getTransactionByHash",
            json!({"hash": id.to_string()}),
        )]);
        assert!(block_on(matching.client().known(&id)).expect("matching hash is known"));
        let mismatched = Script::node(vec![Reply::ok(
            "eth_getTransactionByHash",
            json!({"hash": format!("0x{}", "bb".repeat(32))}),
        )]);
        let error = block_on(mismatched.client().known(&id)).unwrap_err();
        assert!(!error.retryable);
        assert!(error.message.contains("does not match"));
    }
}
