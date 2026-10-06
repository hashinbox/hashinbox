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

#[cfg(test)]
mod tests {
    use base::transaction::Envelope;
    use bitcoin::{
        Amount, OutPoint, ScriptBuf, Sequence, TxIn, TxOut, Txid, Witness, absolute, consensus,
        hashes::Hash,
    };
    use futures_executor::block_on;

    use super::*;
    use crate::native::rpc::script::{Reply, Script, Scripted};

    fn signed() -> SignedTransaction {
        let transaction = bitcoin::Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::new(Txid::from_byte_array([3; 32]), 0),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        SignedTransaction::from_envelope(
            TransactionId::from(transaction.compute_txid()),
            Envelope::new(consensus::serialize(&transaction)),
        )
        .unwrap()
    }

    fn rate() -> FeeRate {
        FeeRate::from_sat_per_kvb(10_000)
    }

    fn broadcast(reply: Reply, max_fee_rate: FeeRate) -> Result<TransactionId, Error> {
        let script = Script::node(vec![reply]);
        block_on(script.client().broadcast(signed(), max_fee_rate))
    }

    #[test]
    fn invalid_fee_ceilings_fail_before_the_wire() {
        for rate in [
            FeeRate::from_sat_per_kvb(0),
            FeeRate::from_sat_per_kvb(100_000_001),
        ] {
            let error =
                block_on(Script::node(Vec::new()).client().broadcast(signed(), rate)).unwrap_err();
            assert_eq!(error.kind, ErrorKind::FeeUnavailable);
        }
    }

    #[test]
    fn broadcast_separates_definite_rejection_from_unknown_outcomes() {
        let id = signed().id;
        assert_eq!(
            broadcast(
                Reply::ok("sendrawtransaction", json!(id.to_string())),
                rate()
            ),
            Ok(id)
        );
        let rejected = broadcast(
            Reply::failure("sendrawtransaction", -26, "rejected"),
            rate(),
        )
        .unwrap_err();
        assert_eq!(rejected.kind, ErrorKind::Rejected);
        assert_eq!(
            rejected.message,
            "Bitcoin JSON-RPC request failed with code -26"
        );
        let timeout = json_rpc::Error {
            kind: json_rpc::ErrorKind::Timeout,
            message: "timed out".to_owned(),
        };
        for reply in [
            Reply::ok("sendrawtransaction", json!(format!("{:064x}", 9))),
            Reply::ok("sendrawtransaction", json!(true)),
            Reply::result("sendrawtransaction", Err(timeout)),
        ] {
            let unknown = broadcast(reply, rate()).unwrap_err();
            assert_eq!(unknown.kind, ErrorKind::RpcUnavailable);
        }
    }

    /// The injected tip probe of a test cluster.
    fn tip(client: &Script) -> BoxFuture<'_, Result<u64, CallError>> {
        Box::pin(async move {
            let probe = json_rpc::Request::new("getblockcount", json!([]));
            let raw = json_rpc::Client::request(client, probe).await??;
            Ok(raw.deserialize()?)
        })
    }

    #[test]
    fn submission_reaches_every_node_of_a_cluster_once() {
        let id = signed().id;
        let height = || Reply::ok("getblockcount", json!(1));
        let timeout = json_rpc::Error {
            kind: json_rpc::ErrorKind::Timeout,
            message: "timed out".to_owned(),
        };
        let nodes = [
            Script::node(vec![
                height(),
                Reply::result("sendrawtransaction", Err(timeout)),
            ]),
            Script::node(vec![
                height(),
                Reply::ok("sendrawtransaction", json!(id.to_string())),
            ]),
        ];
        let cluster = block_on(json_rpc::Cluster::new(
            nodes.to_vec(),
            tip,
            json_rpc::Policy::default(),
        ))
        .unwrap();
        let client = Client::unverified(cluster, crate::native::Network::Regtest);
        assert_eq!(block_on(client.broadcast(signed(), rate())), Ok(id));
        for node in &nodes {
            node.exhausted();
            assert_eq!(node.methods(), ["getblockcount", "sendrawtransaction"]);
        }
    }

    #[test]
    fn known_distinguishes_absent_from_unreadable() {
        let id = signed().id;
        for (reply, expected) in [
            (Reply::ok("getrawtransaction", json!("0200")), Some(true)),
            (
                Reply::failure("getrawtransaction", -5, "No such mempool transaction"),
                Some(false),
            ),
            (Reply::failure("getrawtransaction", -1, "busy"), None),
        ] {
            let script = Script::node(vec![reply.expecting(json!([id.to_string(), false]))]);
            let known = block_on(script.client().known(&id));
            assert_eq!(known.ok(), expected);
        }
    }
}
