use base::{TransactionError, TransactionErrorKind, TransactionId as BaseTransactionId};
use bitcoin::hex::DisplayHex;
use indexing::SourceError;
use serde_json::Value;

use crate::{FeeRate, Satoshi, SignedTransaction, TransactionId};

use super::{
    Client, Preflight,
    error::{map_json_rpc_error, source_error},
    transport::{Client as Transport, RawJson},
    wire::{required_bool, required_string},
};

impl<C> Client<C>
where
    C: Transport,
{
    pub async fn preflight(
        &self,
        transaction: &SignedTransaction,
        max_fee_rate: FeeRate,
    ) -> Result<Preflight, SourceError> {
        let max_fee_rate = max_fee_rate.core_maximum_json()?;
        let raw = self
            .request_result(
                "testmempoolaccept",
                Value::Array(vec![
                    Value::Array(vec![Value::String(
                        transaction.consensus_bytes().to_lower_hex_string(),
                    )]),
                    Value::Number(max_fee_rate),
                ]),
            )
            .await?;
        Preflight::from_rpc_result(&raw, transaction.id())
    }

    pub async fn broadcast(
        &self,
        transaction: SignedTransaction,
        max_fee_rate: FeeRate,
    ) -> Result<TransactionId, TransactionError> {
        let expected_id = transaction.id();
        let max_fee_rate = max_fee_rate
            .core_maximum_json()
            .map_err(|error| TransactionError::new(TransactionErrorKind::Fee, error.to_string()))?;
        let raw = match self
            .request_result_detailed_once(
                "sendrawtransaction",
                Value::Array(vec![
                    Value::String(transaction.consensus_bytes().to_lower_hex_string()),
                    Value::Number(max_fee_rate),
                ]),
            )
            .await
        {
            Ok(raw) => raw,
            Err(failure) if failure.remote_code.is_some() => {
                return Err(TransactionError::new(
                    TransactionErrorKind::Rejected,
                    failure.error.to_string(),
                ));
            }
            Err(failure) => return Err(ambiguous_submission(expected_id, failure.error)),
        };
        let returned: String = raw
            .deserialize()
            .map_err(map_json_rpc_error)
            .map_err(|error| ambiguous_submission(expected_id, error))?;
        let returned = returned.parse::<TransactionId>().map_err(|_| {
            ambiguous_submission(
                expected_id,
                "Bitcoin Core returned an invalid transaction ID",
            )
        })?;
        if returned != expected_id {
            return Err(ambiguous_submission(
                expected_id,
                "Bitcoin Core returned a different transaction ID after broadcast",
            ));
        }
        Ok(returned)
    }
}

impl Preflight {
    fn from_rpc_result(raw: &RawJson, expected_id: TransactionId) -> Result<Self, SourceError> {
        let values: Vec<Value> = raw.deserialize().map_err(map_json_rpc_error)?;
        if values.len() != 1 {
            return Err(source_error(
                "Bitcoin testmempoolaccept returned an unexpected result count",
                true,
            ));
        }
        let result = values[0].as_object().ok_or_else(|| {
            source_error("Bitcoin testmempoolaccept result must be an object", true)
        })?;
        let returned_id = required_string(result, "txid", "Bitcoin preflight transaction ID")?
            .parse::<TransactionId>()
            .map_err(|_| source_error("Bitcoin preflight returned an invalid txid", true))?;
        if returned_id != expected_id {
            return Err(source_error(
                "Bitcoin preflight returned a different transaction ID",
                true,
            ));
        }
        let allowed = required_bool(result, "allowed", "Bitcoin preflight allowance")?;
        let reject_reason = result
            .get("reject-reason")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let virtual_size = result.get("vsize").and_then(Value::as_u64);
        let base_fee = result
            .get("fees")
            .and_then(Value::as_object)
            .and_then(|fees| fees.get("base"))
            .map(|value| Satoshi::from_rpc_json(value, "Bitcoin preflight base fee"))
            .transpose()?;
        Ok(Self {
            allowed,
            reject_reason,
            virtual_size,
            base_fee,
        })
    }
}

// shared Bitcoin RPC uncertainty mapping attaches the verified local transaction ID independently
// of client state
// design-lint: allow unclassified-free-function -- See the boundary rationale above.
fn ambiguous_submission(id: TransactionId, error: impl std::fmt::Display) -> TransactionError {
    TransactionError::new(TransactionErrorKind::Unavailable, error.to_string())
        .with_ambiguous_transaction_id(BaseTransactionId::new(id.to_string()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bitcoin::{Amount, ScriptBuf, Transaction, TxOut, absolute, consensus};
    use futures_executor::block_on;
    use json_rpc::{BoxFuture, Call, CallResult, Error, RawJson};
    use serde_json::json;

    use super::*;
    use crate::{
        Network,
        rpc::{CoreConfig, client::Connection},
    };

    struct Reply {
        params: Value,
        result: Value,
    }

    impl Transport for Reply {
        fn request<'a>(
            &'a self,
            method: &'a str,
            params: Value,
        ) -> BoxFuture<'a, Result<CallResult, Error>> {
            assert_eq!(method, "testmempoolaccept");
            assert_eq!(params, self.params);
            Box::pin(async { Ok(Ok(RawJson::from_serializable(&self.result)?)) })
        }

        fn request_once<'a>(
            &'a self,
            _method: &'a str,
            _params: Value,
        ) -> BoxFuture<'a, Result<CallResult, Error>> {
            panic!("preflight retains ordinary transport execution")
        }

        fn batch<'a>(&'a self, _calls: Vec<Call>) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
            panic!("preflight is one request")
        }
    }

    fn signed_transaction() -> SignedTransaction {
        let transaction = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![],
            output: vec![TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        SignedTransaction::from_consensus_bytes(
            TransactionId::from(transaction.compute_txid()),
            consensus::serialize(&transaction),
        )
        .unwrap()
    }

    fn preflight(result: Value, fee_rate: FeeRate) -> Result<Preflight, SourceError> {
        let transaction = signed_transaction();
        let params = json!([
            [transaction.consensus_bytes().to_lower_hex_string()],
            0.0001
        ]);
        let client = Client {
            connection: Arc::new(Connection {
                client: Reply { params, result },
                config: CoreConfig::new(Network::Regtest),
            }),
        };
        block_on(client.preflight(&transaction, fee_rate))
    }

    #[test]
    fn preflight_preserves_shape_identity_allowance_and_fee_error_order() {
        let id = signed_transaction().id().to_string();
        for (result, message) in [
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
                json!([{"txid": id, "allowed": false, "fees": {"base": "invalid"}}]),
                "Bitcoin preflight base fee must be a JSON number",
            ),
        ] {
            let error = preflight(result, FeeRate::new(10_000)).unwrap_err();
            assert_eq!(error.message, message);
            assert!(error.retryable);
        }
    }

    #[test]
    fn preflight_keeps_optional_field_tolerance_and_exact_rejected_fee() {
        let id = signed_transaction().id().to_string();
        for fields in [
            json!({}),
            json!({"reject-reason": false, "vsize": -1, "fees": false}),
            json!({"reject-reason": null, "vsize": null, "fees": {}}),
        ] {
            let mut result = json!({"txid": id, "allowed": true});
            result
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            assert_eq!(
                preflight(json!([result]), FeeRate::new(10_000)).unwrap(),
                Preflight {
                    allowed: true,
                    reject_reason: None,
                    virtual_size: None,
                    base_fee: None,
                }
            );
        }
        let base_fee: Value = serde_json::from_str("0.00000123").unwrap();
        let result = json!([{
            "txid": id, "allowed": false, "reject-reason": "", "vsize": u64::MAX,
            "fees": {"base": base_fee},
        }]);
        assert_eq!(
            preflight(result, FeeRate::new(10_000)).unwrap(),
            Preflight {
                allowed: false,
                reject_reason: Some(String::new()),
                virtual_size: Some(u64::MAX),
                base_fee: Some(Satoshi(123)),
            }
        );
    }

    #[test]
    fn invalid_preflight_fee_is_rejected_before_transport() {
        for (rate, message) in [
            (0, "Bitcoin maximum fee rate must be greater than zero"),
            (
                100_000_001,
                "Bitcoin maximum fee rate exceeds Bitcoin Core's 1 BTC/kvB limit",
            ),
        ] {
            let error = preflight(Value::Null, FeeRate::new(rate)).unwrap_err();
            assert_eq!(error.message, message);
            assert!(!error.retryable);
        }
    }
}
