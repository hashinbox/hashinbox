use base::BoxFuture;
use serde_json::{Value, json};

use super::{FeeRate, SignedTransaction, TransactionId};
use crate::native::{Error, ErrorKind, Satoshi, json::Object, rpc::Client};

/// Bitcoin Core's `testmempoolaccept` verdict on one transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Acceptance {
    /// Whether the mempool would accept it now.
    pub allowed: bool,
    /// Core's reason when it would not.
    pub reject_reason: Option<String>,
    /// Its virtual size, when reported.
    pub virtual_size: Option<u64>,
    /// Its fee, when reported.
    pub base_fee: Option<Satoshi>,
}

/// The node checks a send needs before anything is submitted: the fee rate
/// to build with, and whether the mempool would take the signed result.
pub trait Preflight: Send + Sync {
    /// Bitcoin Core's conservative fee rate for confirmation within
    /// `target_blocks` blocks, refused above `maximum`.
    fn fee_rate(
        &self,
        target_blocks: u16,
        maximum: FeeRate,
    ) -> BoxFuture<'_, Result<FeeRate, Error>>;

    /// Asks the node whether it would accept `transaction` at or below
    /// `maximum`, without submitting it.
    fn accept<'a>(
        &'a self,
        transaction: &'a SignedTransaction,
        maximum: FeeRate,
    ) -> BoxFuture<'a, Result<Acceptance, Error>>;
}

impl<C: json_rpc::Client> Preflight for Client<C> {
    fn fee_rate(
        &self,
        target_blocks: u16,
        maximum: FeeRate,
    ) -> BoxFuture<'_, Result<FeeRate, Error>> {
        Box::pin(async move { self.estimate(target_blocks).await?.within(maximum) })
    }

    fn accept<'a>(
        &'a self,
        transaction: &'a SignedTransaction,
        maximum: FeeRate,
    ) -> BoxFuture<'a, Result<Acceptance, Error>> {
        Box::pin(async move {
            let params = json!([[transaction.hex()], maximum.core_maximum()?]);
            let reply = self.request("testmempoolaccept", params).await?;
            Acceptance::parse(&reply, transaction.id)
        })
    }
}

impl FeeRate {
    /// This rate, unless it exceeds `maximum`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::FeeUnavailable`] above `maximum`.
    pub fn within(self, maximum: Self) -> Result<Self, Error> {
        if self > maximum {
            return Err(Error::new(
                ErrorKind::FeeUnavailable,
                "estimated Bitcoin fee rate exceeds the configured maximum",
            ));
        }
        Ok(self)
    }
}

impl Acceptance {
    /// Nothing when the mempool would accept the transaction.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Rejected`] otherwise: submitting it would fail.
    pub fn require(&self) -> Result<(), Error> {
        if !self.allowed {
            return Err(Error::new(
                ErrorKind::Rejected,
                "Bitcoin node rejected transaction preflight",
            ));
        }
        Ok(())
    }

    fn parse(reply: &Value, expected: TransactionId) -> Result<Self, Error> {
        let [result] = reply.as_array().map(Vec::as_slice).unwrap_or_default() else {
            return Err(Error::invalid_reply(
                "Bitcoin testmempoolaccept returned an unexpected result count",
            ));
        };
        let result = Object::new(
            result,
            "Bitcoin testmempoolaccept result",
            Error::invalid_reply,
        )?;
        let returned = result
            .string("txid", "Bitcoin preflight transaction ID")?
            .parse::<TransactionId>()
            .map_err(|_| result.fail("Bitcoin preflight returned an invalid txid"))?;
        if returned != expected {
            return Err(result.fail("Bitcoin preflight returned a different transaction ID"));
        }
        let allowed = result.bool("allowed", "Bitcoin preflight allowance")?;
        let base_fee = result
            .get("fees")
            .and_then(|fees| fees.get("base"))
            .map(|fee| Satoshi::from_btc(fee, "Bitcoin preflight base fee"))
            .transpose()
            .map_err(Error::invalid_reply)?;
        Ok(Self {
            allowed,
            reject_reason: result
                .get("reject-reason")
                .and_then(Value::as_str)
                .map(str::to_owned),
            virtual_size: result.get("vsize").and_then(Value::as_u64),
            base_fee,
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

    fn preflight(reply: Value) -> Result<Acceptance, Error> {
        let transaction = signed();
        let script = Script::node(vec![
            Reply::ok("testmempoolaccept", reply).expecting(json!([[transaction.hex()], 0.0001])),
        ]);
        block_on(
            script
                .client()
                .accept(&transaction, FeeRate::from_sat_per_kvb(10_000)),
        )
    }

    #[test]
    fn preflight_keeps_shape_identity_allowance_and_fee_error_order() {
        let id = signed().id.to_string();
        for (reply, message) in [
            (
                json!([]),
                "Bitcoin testmempoolaccept returned an unexpected result count",
            ),
            (
                json!([null, null]),
                "Bitcoin testmempoolaccept returned an unexpected result count",
            ),
            (
                json!([null]),
                "Bitcoin testmempoolaccept result must be an object",
            ),
            (
                json!([{"txid": false}]),
                "Bitcoin preflight transaction ID is missing or invalid",
            ),
            (
                json!([{"txid": "invalid"}]),
                "Bitcoin preflight returned an invalid txid",
            ),
            (
                json!([{"txid": "03".repeat(32)}]),
                "Bitcoin preflight returned a different transaction ID",
            ),
            (
                json!([{"txid": id, "fees": {"base": -1}}]),
                "Bitcoin preflight allowance is missing or invalid",
            ),
            (
                json!([{"txid": id, "allowed": false, "fees": {"base": "x"}}]),
                "Bitcoin preflight base fee must be a JSON number",
            ),
        ] {
            let error = preflight(reply).unwrap_err();
            assert_eq!((error.message.as_str(), error.retryable), (message, true));
        }
    }

    #[test]
    fn preflight_keeps_optional_fields_and_the_exact_rejected_fee() {
        let id = signed().id.to_string();
        for fields in [
            json!({}),
            json!({"reject-reason": false, "vsize": -1, "fees": false}),
        ] {
            let mut result = json!({"txid": id, "allowed": true});
            result
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            let acceptance = preflight(json!([result])).unwrap();
            assert!(acceptance.allowed);
            assert_eq!(
                (
                    acceptance.reject_reason,
                    acceptance.virtual_size,
                    acceptance.base_fee
                ),
                (None, None, None)
            );
        }
        let fee: Value = serde_json::from_str("0.00000123").unwrap();
        let acceptance = preflight(json!([{
            "txid": id, "allowed": false, "reject-reason": "missing-inputs", "vsize": 82,
            "fees": {"base": fee},
        }]))
        .unwrap();
        assert_eq!(acceptance.reject_reason.as_deref(), Some("missing-inputs"));
        assert_eq!(
            (acceptance.virtual_size, acceptance.base_fee),
            (Some(82), Some(Satoshi::from_atomic(123)))
        );
    }

    #[test]
    fn fee_rates_above_the_ceiling_and_invalid_ceilings_fail() {
        let script = Script::node(vec![Reply::ok(
            "estimatesmartfee",
            json!({"feerate": 0.00002, "blocks": 6}),
        )]);
        let error = block_on(
            script
                .client()
                .fee_rate(6, FeeRate::from_sat_per_kvb(1_999)),
        )
        .unwrap_err();
        assert_eq!(
            (error.kind, error.message.as_str()),
            (
                ErrorKind::FeeUnavailable,
                "estimated Bitcoin fee rate exceeds the configured maximum"
            )
        );
        assert_eq!(
            FeeRate::from_sat_per_kvb(2_000).within(FeeRate::from_sat_per_kvb(2_000)),
            Ok(FeeRate::from_sat_per_kvb(2_000))
        );
        for rate in [
            FeeRate::from_sat_per_kvb(0),
            FeeRate::from_sat_per_kvb(100_000_001),
        ] {
            let error =
                block_on(Script::node(Vec::new()).client().accept(&signed(), rate)).unwrap_err();
            assert_eq!(error.kind, ErrorKind::FeeUnavailable);
        }
        let rejected = Acceptance {
            allowed: false,
            reject_reason: None,
            virtual_size: None,
            base_fee: None,
        };
        assert_eq!(rejected.require().unwrap_err().kind, ErrorKind::Rejected);
    }
}
