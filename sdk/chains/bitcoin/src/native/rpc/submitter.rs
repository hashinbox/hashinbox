//! Submission: the chain's only external effect, and the read that judges it.
//!
//! `Submitter::broadcast` sends one exact envelope with one visible attempt;
//! `known` asks Bitcoin Core whether it holds that exact txid, which is the
//! only way to settle an attempt whose outcome the node never reported.

use base::BoxFuture;
use json_rpc::CallError;
use serde_json::json;

use super::Client;
use crate::native::{Error, ErrorKind, SignedTransaction, TransactionId, transaction::FeeRate};

/// `RPC_INVALID_ADDRESS_OR_KEY`: Bitcoin Core does not have the transaction.
const NOT_FOUND: i64 = -5;

/// Transaction submission: the chain's only external effect.
pub trait Submitter: Send + Sync {
    /// Submits one exact signed envelope with one visible attempt at every
    /// node the transport reaches (`json_rpc::Client::broadcast`) and
    /// verifies the returned txid.
    ///
    /// A definitive rejection is [`ErrorKind::Rejected`] (or
    /// [`ErrorKind::FeeUnavailable`] before the wire). Any
    /// [`ErrorKind::RpcUnavailable`] means the attempt may have reached the
    /// node: the outcome is unknown and only `transaction.id` identifies it.
    fn broadcast<'a>(
        &'a self,
        transaction: SignedTransaction,
        max_fee_rate: FeeRate,
    ) -> BoxFuture<'a, Result<TransactionId, Error>>;

    /// Whether the node has the exact transaction `id`: in its mempool, or,
    /// with `-txindex`, in its chain.
    fn known<'a>(&'a self, id: &'a TransactionId) -> BoxFuture<'a, Result<bool, Error>>;
}

impl<C: json_rpc::Client> Submitter for Client<C> {
    fn broadcast<'a>(
        &'a self,
        transaction: SignedTransaction,
        max_fee_rate: FeeRate,
    ) -> BoxFuture<'a, Result<TransactionId, Error>> {
        Box::pin(async move {
            let params = json!([transaction.hex(), max_fee_rate.core_maximum()?]);
            let raw = match self.broadcast_once("sendrawtransaction", params).await {
                Ok(raw) => raw,
                Err(CallError::Remote(failure)) => {
                    return Err(Error::new(
                        ErrorKind::Rejected,
                        format!("Bitcoin JSON-RPC request failed with code {}", failure.code),
                    ));
                }
                Err(CallError::Transport(error)) => return Err(Error::from(error)),
            };
            let unknown = |message: &str| Error::rpc(message, false);
            let returned: String = raw.deserialize()?;
            let returned = returned
                .parse::<TransactionId>()
                .map_err(|_| unknown("Bitcoin Core returned an invalid transaction ID"))?;
            if returned != transaction.id {
                return Err(unknown(
                    "Bitcoin Core returned a different transaction ID after broadcast",
                ));
            }
            Ok(returned)
        })
    }

    fn known<'a>(&'a self, id: &'a TransactionId) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move {
            let params = json!([id.to_string(), false]);
            match self.call_once("getrawtransaction", params).await {
                Ok(_) => Ok(true),
                Err(CallError::Remote(failure)) if failure.code == NOT_FOUND => Ok(false),
                Err(CallError::Remote(failure)) => Err(Error::rpc(
                    format!("Bitcoin JSON-RPC request failed with code {}", failure.code),
                    true,
                )),
                Err(CallError::Transport(error)) => Err(Error::from(error)),
            }
        })
    }
}
