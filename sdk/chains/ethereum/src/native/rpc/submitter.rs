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
    /// Implementations make one visible submission attempt at every node
    /// the transport reaches (`json_rpc::Client::broadcast`). Any
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
                .broadcast_once(method, json!([hex::encode_prefixed(bytes)]))
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
