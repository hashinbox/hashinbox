use alloy_primitives::{hex, keccak256};
use base::{TransactionError, TransactionErrorKind, TransactionId as BaseTransactionId};
use indexing::{BoxFuture, SourceError};
use serde_json::{Map, Value, json};

use super::{
    Client, Limits,
    accounts::{AccountClient, Accounts},
    blocks::{Methods, ProductionClient},
    error::BuildError,
    transport::Client as Transport,
    wire::{
        CallError, invalid_rpc_response, map_json_rpc_error, parse_fixed_data, parse_quantity_u64,
    },
};
use crate::{
    AssetKind, BuildContext, ChainError, ChainErrorKind, SignedTransaction, TransactionId,
    TransferRequest, Wei, erc20,
};

pub type HttpAccounts = AccountClient<ProductionClient>;
pub type HttpTransactions = TransactionClient<ProductionClient>;

/// Focused transaction calls over a shared RPC client.
pub struct TransactionClient<C> {
    methods: Methods<C>,
}

impl<C> TransactionClient<C> {
    pub fn new(
        client: Client<C>,
        expected_chain_id: u64,
        limits: Limits,
    ) -> Result<Self, BuildError> {
        Methods::from_client(client, expected_chain_id, Some(limits))
            .map(|methods| Self { methods })
    }

    pub(super) fn from_methods(methods: Methods<C>) -> Self {
        Self { methods }
    }
}

/// Transaction preparation and submission.
pub trait Transactions: Send + Sync {
    /// Preflights a transfer using the nonce reserved by the caller.
    fn build_context<'a>(
        &'a self,
        request: &'a TransferRequest,
        nonce: u64,
    ) -> BoxFuture<'a, Result<BuildContext, ChainError>>;

    /// Submits one exact signed envelope and verifies the returned hash.
    ///
    /// Implementations must use one visible submission attempt. A definitive
    /// pre-wire or pre-acceptance rejection stays ID-free. Transport failures
    /// and malformed, missing, or mismatched responses after attempting the
    /// envelope must carry only the ID derived from those exact local bytes.
    fn broadcast<'a>(
        &'a self,
        transaction: SignedTransaction,
    ) -> BoxFuture<'a, Result<TransactionId, TransactionError>>;

    /// Reports whether the node exposes the exact requested transaction hash.
    fn known<'a>(
        &'a self,
        transaction: &'a TransactionId,
    ) -> BoxFuture<'a, Result<bool, SourceError>>;
}

impl<C> Transactions for Methods<C>
where
    C: Transport,
{
    fn build_context<'a>(
        &'a self,
        request: &'a TransferRequest,
        nonce: u64,
    ) -> BoxFuture<'a, Result<BuildContext, ChainError>> {
        Box::pin(async move {
            let input = request.input();
            let limits = self.limits().map_err(ChainError::from_rpc)?;
            if input.len() > limits.max_input_bytes() {
                return Err(ChainError::new(
                    ChainErrorKind::InvalidTransaction,
                    "Ethereum transaction input exceeds the configured size limit",
                ));
            }

            self.verify_transaction_chain_id().await?;
            let mut transaction = Map::new();
            transaction.insert("from".to_owned(), json!(request.from().to_string()));
            transaction.insert("to".to_owned(), json!(request.to().to_string()));
            transaction.insert("value".to_owned(), json!(request.value().to_quantity()));
            transaction.insert("data".to_owned(), json!(hex::encode_prefixed(&input)));

            if request.erc20_transfer().is_some() {
                self.ensure_token_amount(request).await?;
                self.simulate_token_transfer(Value::Object(transaction.clone()))
                    .await?;
            }

            let estimated_gas_limit = self.estimate_gas(Value::Object(transaction)).await?;
            let context = self.fee_context(limits, estimated_gas_limit, nonce).await?;
            let total_fee = limits.total_fee(context.gas_limit, &context.max_fee_per_gas)?;
            self.ensure_native_funds(request, &total_fee).await?;
            Ok(context)
        })
    }

    fn broadcast<'a>(
        &'a self,
        transaction: SignedTransaction,
    ) -> BoxFuture<'a, Result<TransactionId, TransactionError>> {
        Box::pin(async move {
            let computed = TransactionId(keccak256(&transaction.envelope).0);
            if computed != transaction.id {
                return Err(TransactionError::new(
                    TransactionErrorKind::InvalidTransaction,
                    "signed Ethereum envelope hash does not match its transaction ID",
                ));
            }
            let result = self
                .request_result_detailed_once(
                    "eth_sendRawTransaction",
                    json!([hex::encode_prefixed(&transaction.envelope)]),
                )
                .await;
            let raw = match result {
                Ok(raw) => Some(raw),
                Err(error) if error.is_already_known() => None,
                Err(CallError::Remote(failure)) => {
                    return Err(ambiguous_submission(
                        &computed,
                        CallError::Remote(failure).into_source("eth_sendRawTransaction"),
                    ));
                }
                Err(CallError::Local(error)) => {
                    return Err(ambiguous_submission(&computed, error));
                }
            };
            let Some(raw) = raw else {
                return self.confirmed_submission(computed).await;
            };
            let returned: String = raw
                .deserialize()
                .map_err(map_json_rpc_error)
                .map_err(|error| ambiguous_submission(&computed, error))?;
            let returned = TransactionId::from_rpc(&returned, "eth_sendRawTransaction")
                .map_err(|error| ambiguous_submission(&computed, error))?;
            if returned != computed {
                return Err(ambiguous_submission(
                    &computed,
                    "Ethereum submission response hash differs from the exact signed \
                     envelope; outcome is ambiguous",
                ));
            }
            Ok(returned)
        })
    }

    fn known<'a>(
        &'a self,
        transaction: &'a TransactionId,
    ) -> BoxFuture<'a, Result<bool, SourceError>> {
        Box::pin(async move { self.confirm_known_transaction(transaction).await })
    }
}

impl<C> Methods<C>
where
    C: Transport,
{
    async fn confirmed_submission(
        &self,
        computed: TransactionId,
    ) -> Result<TransactionId, TransactionError> {
        match self.confirm_known_transaction(&computed).await {
            Ok(true) => Ok(computed),
            Ok(false) => Err(ambiguous_submission(
                &computed,
                "Ethereum RPC reported an already-known transaction but did \
                 not expose the matching hash",
            )),
            Err(error) => Err(ambiguous_submission(&computed, error)),
        }
    }

    async fn fee_context(
        &self,
        limits: &Limits,
        estimated: u64,
        nonce: u64,
    ) -> Result<BuildContext, ChainError> {
        let gas_limit = limits.gas_limit(estimated)?;
        let max_priority_fee_per_gas = self
            .rpc_wei("eth_maxPriorityFeePerGas", json!([]))
            .await
            .map_err(ChainError::from_rpc)?;
        limits.validate_priority_fee(&max_priority_fee_per_gas)?;
        let base_fee = self.latest_base_fee().await?;
        let max_fee_per_gas = limits.max_fee(&base_fee, &max_priority_fee_per_gas)?;
        Ok(BuildContext {
            chain_id: self.expected_chain_id,
            nonce,
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
        })
    }

    async fn latest_base_fee(&self) -> Result<Wei, ChainError> {
        let latest_block = self
            .request_result("eth_getBlockByNumber", json!(["latest", false]))
            .await
            .map_err(ChainError::from_rpc)?;
        let latest_block: Value = latest_block
            .deserialize()
            .map_err(map_json_rpc_error)
            .map_err(ChainError::from_rpc)?;
        let base_fee = latest_block
            .get("baseFeePerGas")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ChainError::from_rpc(invalid_rpc_response(
                    "eth_getBlockByNumber",
                    "latest block has no EIP-1559 baseFeePerGas",
                ))
            })?;
        Wei::from_quantity(base_fee)
            .map_err(|message| invalid_rpc_response("eth_getBlockByNumber", message))
            .map_err(ChainError::from_rpc)
    }

    async fn verify_transaction_chain_id(&self) -> Result<(), ChainError> {
        let actual = self
            .rpc_u64("eth_chainId", json!([]))
            .await
            .map_err(ChainError::from_rpc)?;
        if actual != self.expected_chain_id {
            return Err(ChainError::new(
                ChainErrorKind::Divergent,
                format!(
                    "Ethereum RPC chain ID {actual} does not match configured chain ID {}",
                    self.expected_chain_id
                ),
            ));
        }
        Ok(())
    }

    async fn estimate_gas(&self, transaction: Value) -> Result<u64, ChainError> {
        let raw = match self
            .request_result_detailed("eth_estimateGas", json!([transaction]))
            .await
        {
            Ok(raw) => raw,
            Err(error) => {
                if let Some(code) = error.execution_revert_code() {
                    return Err(ChainError::new(
                        ChainErrorKind::Rejected,
                        format!("Ethereum gas estimation was rejected with code {code}"),
                    ));
                }
                return Err(ChainError::from_rpc(error.into_source("eth_estimateGas")));
            }
        };
        let value: String = raw
            .deserialize()
            .map_err(map_json_rpc_error)
            .map_err(ChainError::from_rpc)?;
        parse_quantity_u64(&value)
            .map_err(|message| invalid_rpc_response("eth_estimateGas", message))
            .map_err(ChainError::from_rpc)
    }

    async fn simulate_token_transfer(&self, transaction: Value) -> Result<(), ChainError> {
        let raw = match self
            .request_result_detailed("eth_call", json!([transaction, "pending"]))
            .await
        {
            Ok(raw) => raw,
            Err(error) => {
                if let Some(code) = error.execution_revert_code() {
                    let message = format!(
                        "Ethereum ERC-20 transfer simulation was rejected with code {code}"
                    );
                    return Err(ChainError::new(ChainErrorKind::Rejected, message));
                }
                return Err(ChainError::from_rpc(error.into_source("eth_call")));
            }
        };
        let value: String = raw.deserialize().map_err(|_| {
            ChainError::new(
                ChainErrorKind::Rejected,
                "Ethereum ERC-20 transfer simulation returned an invalid JSON value",
            )
        })?;
        let word = parse_fixed_data::<32>(&value, "ERC-20 transfer result").map_err(|message| {
            ChainError::new(
                ChainErrorKind::Rejected,
                format!("Ethereum ERC-20 transfer simulation returned invalid data: {message}"),
            )
        })?;
        let transferred = erc20::decode_transfer(&word).map_err(|_| {
            ChainError::new(
                ChainErrorKind::Rejected,
                "Ethereum ERC-20 transfer simulation returned an invalid ABI result",
            )
        })?;
        if !transferred {
            return Err(ChainError::new(
                ChainErrorKind::Rejected,
                "Ethereum ERC-20 transfer simulation returned false",
            ));
        }
        Ok(())
    }

    async fn ensure_token_amount(&self, request: &TransferRequest) -> Result<(), ChainError> {
        let Some((token, amount)) = request.erc20_transfer() else {
            return Ok(());
        };
        let token_asset = AssetKind::Erc20(token.clone());
        let token_balance = self
            .balance(request.from().clone(), &token_asset, None)
            .await
            .map_err(ChainError::from_rpc)?;
        if token_balance < *amount {
            return Err(ChainError::new(
                ChainErrorKind::InsufficientFunds,
                "Ethereum ERC-20 balance is insufficient for the transfer amount",
            ));
        }
        Ok(())
    }

    async fn ensure_native_funds(
        &self,
        request: &TransferRequest,
        total_fee: &crate::Wei,
    ) -> Result<(), ChainError> {
        let native = AssetKind::Native;
        let native_balance = self
            .balance(request.from().clone(), &native, None)
            .await
            .map_err(ChainError::from_rpc)?;
        if request.erc20_transfer().is_none() {
            let required = request.value().checked_add(total_fee).ok_or_else(|| {
                ChainError::new(
                    ChainErrorKind::FeeUnavailable,
                    "Ethereum transfer value plus maximum fee overflowed U256",
                )
            })?;
            if native_balance < required {
                return Err(ChainError::new(
                    ChainErrorKind::InsufficientFunds,
                    "Ethereum native balance is insufficient for transfer value and maximum fee",
                ));
            }
            return Ok(());
        }

        if native_balance < *total_fee {
            return Err(ChainError::new(
                ChainErrorKind::InsufficientFunds,
                "Ethereum native balance is insufficient for the ERC-20 maximum fee",
            ));
        }
        Ok(())
    }
}

impl<C> Transactions for TransactionClient<C>
where
    C: Transport,
{
    fn build_context<'a>(
        &'a self,
        request: &'a TransferRequest,
        nonce: u64,
    ) -> BoxFuture<'a, Result<BuildContext, ChainError>> {
        self.methods.build_context(request, nonce)
    }

    fn broadcast<'a>(
        &'a self,
        transaction: SignedTransaction,
    ) -> BoxFuture<'a, Result<TransactionId, TransactionError>> {
        self.methods.broadcast(transaction)
    }

    fn known<'a>(
        &'a self,
        transaction: &'a TransactionId,
    ) -> BoxFuture<'a, Result<bool, SourceError>> {
        self.methods.known(transaction)
    }
}

// RPC boundary adapter adds Ethereum ambiguity context and the exact local transaction ID to
// uncertain submission failures without client state
// design-lint: allow unclassified-free-function -- See the boundary rationale above.
fn ambiguous_submission(id: &TransactionId, error: impl std::fmt::Display) -> TransactionError {
    TransactionError::new(
        TransactionErrorKind::Unavailable,
        format!("Ethereum submission outcome is ambiguous: {error}"),
    )
    .with_ambiguous_transaction_id(BaseTransactionId::new(id.to_string()))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use futures_executor::block_on;
    use json_rpc::{Call, CallResult, Error, Failure, RawJson};

    use super::*;
    use crate::Address;

    struct Step {
        method: &'static str,
        params: Value,
        response: Result<CallResult, Error>,
    }

    impl Step {
        fn value(method: &'static str, params: Value, value: Value) -> Self {
            Self {
                method,
                params,
                response: Ok(Ok(RawJson::from_serializable(&value).unwrap())),
            }
        }
    }

    #[derive(Clone)]
    struct Script(Arc<Mutex<VecDeque<Step>>>);

    impl Script {
        fn new(steps: Vec<Step>) -> Self {
            Self(Arc::new(Mutex::new(steps.into())))
        }

        fn take(&self, method: &str, params: Value) -> Result<CallResult, Error> {
            let step = self.0.lock().unwrap().pop_front().expect("unexpected RPC");
            assert_eq!(method, step.method);
            assert_eq!(params, step.params);
            step.response
        }

        fn exhausted(&self) {
            assert!(self.0.lock().unwrap().is_empty(), "missing RPC");
        }
    }

    impl Transport for Script {
        fn request<'a>(
            &'a self,
            method: &'a str,
            params: Value,
        ) -> BoxFuture<'a, Result<CallResult, Error>> {
            assert_ne!(
                method, "eth_sendRawTransaction",
                "submission must be once-only"
            );
            Box::pin(async move { self.take(method, params) })
        }

        fn request_once<'a>(
            &'a self,
            method: &'a str,
            params: Value,
        ) -> BoxFuture<'a, Result<CallResult, Error>> {
            assert_eq!(method, "eth_sendRawTransaction");
            Box::pin(async move { self.take(method, params) })
        }

        fn batch<'a>(&'a self, _calls: Vec<Call>) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
            panic!("transaction RPCs remain sequential")
        }
    }

    fn limits() -> Limits {
        Limits::new(
            1024,
            0,
            10,
            Wei::from_u128(100),
            Wei::from_u128(10),
            Wei::from_u128(100),
        )
        .unwrap()
    }

    fn native() -> TransferRequest {
        TransferRequest::native_atomic(Address([1; 20]), Address([2; 20]), Wei::from_u128(7))
    }

    fn preparation_steps() -> Vec<Step> {
        let transaction = json!({"from": Address([1; 20]).to_string(),
            "to": Address([2; 20]).to_string(), "value": "0x7", "data": "0x"});
        vec![
            Step::value("eth_chainId", json!([]), json!("0x7a69")),
            Step::value("eth_estimateGas", json!([transaction]), json!("0x2")),
            Step::value("eth_maxPriorityFeePerGas", json!([]), json!("0x2")),
            Step::value(
                "eth_getBlockByNumber",
                json!(["latest", false]),
                json!({"baseFeePerGas": "0x3"}),
            ),
            Step::value(
                "eth_getBalance",
                json!([Address([1; 20]).to_string(), "pending"]),
                json!("0xffff"),
            ),
        ]
    }

    #[test]
    fn fee_failures_stop_before_later_observations_and_keep_exact_errors() {
        for (length, replacement, kind, message) in [
            (
                2,
                json!("0x0"),
                ChainErrorKind::RpcUnavailable,
                "Ethereum RPC eth_estimateGas returned an invalid response: \
                 estimated gas limit is zero",
            ),
            (
                2,
                json!("0xb"),
                ChainErrorKind::FeeUnavailable,
                "Ethereum estimated gas limit exceeds the configured ceiling",
            ),
            (
                3,
                json!("0xb"),
                ChainErrorKind::FeeUnavailable,
                "Ethereum priority fee exceeds the configured ceiling",
            ),
            (
                4,
                json!({}),
                ChainErrorKind::RpcUnavailable,
                "Ethereum RPC eth_getBlockByNumber returned an invalid response: \
                 latest block has no EIP-1559 baseFeePerGas",
            ),
            (
                4,
                json!({"baseFeePerGas": "0x32"}),
                ChainErrorKind::FeeUnavailable,
                "Ethereum max fee per gas exceeds the configured ceiling",
            ),
            (
                4,
                json!({"baseFeePerGas": "0x31"}),
                ChainErrorKind::FeeUnavailable,
                "Ethereum maximum transaction fee exceeds the configured ceiling",
            ),
        ] {
            let mut steps = preparation_steps();
            steps.truncate(length);
            steps[length - 1].response = Ok(Ok(RawJson::from_serializable(&replacement).unwrap()));
            let script = Script::new(steps);
            let methods = Methods::with_client(script.clone(), 31_337, limits()).unwrap();
            let error = block_on(methods.build_context(&native(), 99)).unwrap_err();
            assert_eq!(error.kind, kind);
            assert_eq!(error.message, message);
            script.exhausted();
        }
    }

    #[test]
    fn prepared_fees_preserve_reserved_nonce_without_a_nonce_lookup() {
        let script = Script::new(preparation_steps());
        let methods = Methods::with_client(script.clone(), 31_337, limits()).unwrap();
        assert_eq!(
            block_on(methods.build_context(&native(), u64::MAX)).unwrap(),
            BuildContext {
                chain_id: 31_337,
                nonce: u64::MAX,
                gas_limit: 2,
                max_fee_per_gas: Wei::from_u128(8),
                max_priority_fee_per_gas: Wei::from_u128(2),
            }
        );
        script.exhausted();
    }

    #[test]
    fn fee_overflows_fail_before_the_native_balance_read() {
        for (base_fee, kind, message) in [
            (
                format!("0x{}", "ff".repeat(32)),
                ChainErrorKind::RpcUnavailable,
                "Ethereum RPC eth_getBlockByNumber returned an invalid response: \
                 fee calculation overflowed U256",
            ),
            (
                format!("0x7{}", "f".repeat(63)),
                ChainErrorKind::FeeUnavailable,
                "Ethereum maximum transaction fee overflowed U256",
            ),
        ] {
            let mut steps = preparation_steps();
            steps.truncate(4);
            steps[2].response = Ok(Ok(RawJson::from_serializable(&json!("0x1")).unwrap()));
            let latest = json!({"baseFeePerGas": base_fee});
            steps[3].response = Ok(Ok(RawJson::from_serializable(&latest).unwrap()));
            let script = Script::new(steps);
            let limits = Limits::new(
                1024,
                0,
                10,
                Wei([255; 32]),
                Wei::from_u128(10),
                Wei([255; 32]),
            )
            .unwrap();
            let methods = Methods::with_client(script.clone(), 31_337, limits).unwrap();
            let error = block_on(methods.build_context(&native(), 99)).unwrap_err();
            assert_eq!(error.kind, kind);
            assert_eq!(error.message, message);
            script.exhausted();
        }
    }

    #[test]
    fn already_known_uses_one_submission_and_one_lookup_without_rebroadcast() {
        let envelope = vec![2, 3, 4];
        let id = TransactionId(keccak256(&envelope).0);
        for visible in [true, false] {
            let known = if visible {
                json!({"hash": id.to_string()})
            } else {
                Value::Null
            };
            let submission = Step {
                method: "eth_sendRawTransaction",
                params: json!([hex::encode_prefixed(&envelope)]),
                response: Ok(Err(Failure {
                    code: -32_000,
                    message: "already known".into(),
                    data: None,
                })),
            };
            let script = Script::new(vec![
                submission,
                Step::value("eth_getTransactionByHash", json!([id.to_string()]), known),
            ]);
            let methods = Methods::with_client(script.clone(), 31_337, limits()).unwrap();
            let result = block_on(methods.broadcast(SignedTransaction {
                id: id.clone(),
                envelope: envelope.clone(),
            }));
            if visible {
                assert_eq!(result.unwrap(), id);
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.kind, TransactionErrorKind::Unavailable);
                assert_eq!(
                    error.ambiguous_transaction_id,
                    Some(BaseTransactionId::new(id.to_string()))
                );
                assert_eq!(
                    error.message,
                    "Ethereum submission outcome is ambiguous: Ethereum RPC \
                    reported an already-known transaction but did not expose the matching hash"
                );
            }
            script.exhausted();
        }
    }

    #[test]
    fn malformed_local_hash_never_reaches_submission() {
        let script = Script::new(vec![]);
        let methods = Methods::with_client(script.clone(), 31_337, limits()).unwrap();
        let error = block_on(methods.broadcast(SignedTransaction {
            id: TransactionId([0; 32]),
            envelope: vec![2, 3, 4],
        }))
        .unwrap_err();
        assert_eq!(error.kind, TransactionErrorKind::InvalidTransaction);
        assert_eq!(
            error.message,
            "signed Ethereum envelope hash does not match its transaction ID"
        );
        assert_eq!(error.ambiguous_transaction_id, None);
        script.exhausted();
    }
}
