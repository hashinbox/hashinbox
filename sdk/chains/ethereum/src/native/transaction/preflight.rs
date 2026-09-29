use alloy_primitives::hex;
use base::BoxFuture;
use serde_json::{Map, Value, json};

use super::{BuildContext, FeeLimits, Transfer};
use crate::native::{
    Address, AssetKind, Error, ErrorKind, Wei, erc20,
    rpc::{Accounts, Client, Remote, View, parse_fixed_data, parse_quantity_u64},
};

/// Turns a sender's transfer and its reserved nonce into a checked
/// [`BuildContext`].
///
/// This is transaction policy composed from node reads: input size, chain
/// identity, ERC-20 simulation, gas estimation, fee ceilings, and funds.
pub trait Preflight: Send + Sync {
    /// Checks `transfer` from `from` against `limits` and the node's
    /// current state.
    fn context<'a>(
        &'a self,
        from: &'a Address,
        transfer: &'a Transfer,
        nonce: u64,
        limits: &'a FeeLimits,
    ) -> BoxFuture<'a, Result<BuildContext, Error>>;
}

impl<C: json_rpc::Client> Preflight for Client<C> {
    fn context<'a>(
        &'a self,
        from: &'a Address,
        transfer: &'a Transfer,
        nonce: u64,
        limits: &'a FeeLimits,
    ) -> BoxFuture<'a, Result<BuildContext, Error>> {
        Box::pin(async move {
            let input = transfer.input();
            if input.len() > limits.max_input_bytes() {
                return Err(Error::new(
                    ErrorKind::InvalidTransaction,
                    "Ethereum transaction input exceeds the configured size limit",
                ));
            }
            self.verify_chain_id().await?;
            let mut call = Map::new();
            call.insert("from".to_owned(), json!(from.to_string()));
            call.insert("to".to_owned(), json!(transfer.target().to_string()));
            call.insert("value".to_owned(), json!(transfer.value().to_quantity()));
            call.insert("data".to_owned(), json!(hex::encode_prefixed(&input)));
            if let AssetKind::Erc20(_) = transfer.asset {
                self.ensure_token_amount(from, transfer).await?;
                self.simulate_token_transfer(Value::Object(call.clone()))
                    .await?;
            }
            let estimated = self.estimate_gas(Value::Object(call)).await?;
            let context = self.fee_context(from, limits, estimated, nonce).await?;
            let total_fee = limits.total_fee(context.gas_limit, &context.max_fee_per_gas)?;
            self.ensure_native_funds(from, transfer, &total_fee).await?;
            Ok(context)
        })
    }
}

impl<C: json_rpc::Client> Client<C> {
    async fn fee_context(
        &self,
        from: &Address,
        limits: &FeeLimits,
        estimated: u64,
        nonce: u64,
    ) -> Result<BuildContext, Error> {
        let gas_limit = limits.gas_limit(estimated)?;
        let max_priority_fee_per_gas = self.rpc_wei("eth_maxPriorityFeePerGas", json!([])).await?;
        limits.validate_priority_fee(&max_priority_fee_per_gas)?;
        let base_fee = self.latest_base_fee().await?;
        let max_fee_per_gas = limits.max_fee(&base_fee, &max_priority_fee_per_gas)?;
        Ok(BuildContext {
            chain_id: self.network().chain_id,
            from: from.clone(),
            nonce,
            gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
        })
    }

    async fn latest_base_fee(&self) -> Result<Wei, Error> {
        let method = "eth_getBlockByNumber";
        let latest: Value = self
            .request(method, json!(["latest", false]))
            .await?
            .deserialize()?;
        let base_fee = latest
            .get("baseFeePerGas")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                Error::invalid_response(method, "latest block has no EIP-1559 baseFeePerGas")
            })?;
        Wei::from_quantity(base_fee).map_err(|message| Error::invalid_response(method, message))
    }

    async fn estimate_gas(&self, call: Value) -> Result<u64, Error> {
        let method = "eth_estimateGas";
        let raw = self
            .call(method, json!([call]))
            .await
            .map_err(|error| Self::rejected_or(method, error, "gas estimation"))?;
        let value: String = raw.deserialize()?;
        parse_quantity_u64(&value).map_err(|message| Error::invalid_response(method, message))
    }

    async fn simulate_token_transfer(&self, call: Value) -> Result<(), Error> {
        let rejected = |message: &str| Error::new(ErrorKind::Rejected, message);
        let raw = self
            .call("eth_call", json!([call, "pending"]))
            .await
            .map_err(|error| Self::rejected_or("eth_call", error, "ERC-20 transfer simulation"))?;
        let value: String = raw.deserialize().map_err(|_| {
            rejected("Ethereum ERC-20 transfer simulation returned an invalid JSON value")
        })?;
        let word = parse_fixed_data::<32>(&value).map_err(|message| {
            rejected(&format!(
                "Ethereum ERC-20 transfer simulation returned invalid data: {message}"
            ))
        })?;
        let transferred = erc20::decode_transfer(&word).map_err(|_| {
            rejected("Ethereum ERC-20 transfer simulation returned an invalid ABI result")
        })?;
        if !transferred {
            return Err(rejected(
                "Ethereum ERC-20 transfer simulation returned false",
            ));
        }
        Ok(())
    }

    /// An EVM revert is a definitive rejection; anything else is a node failure.
    fn rejected_or(method: &str, error: json_rpc::CallError, action: &str) -> Error {
        match error.revert_code() {
            Some(code) => Error::new(
                ErrorKind::Rejected,
                format!("Ethereum {action} was rejected with code {code}"),
            ),
            None => Error::from_call(method, error),
        }
    }

    async fn ensure_token_amount(&self, from: &Address, transfer: &Transfer) -> Result<(), Error> {
        let balance = self
            .balance(from.clone(), &transfer.asset, View::Pending)
            .await?;
        if balance < transfer.amount {
            return Err(Error::new(
                ErrorKind::InsufficientFunds,
                "Ethereum ERC-20 balance is insufficient for the transfer amount",
            ));
        }
        Ok(())
    }

    async fn ensure_native_funds(
        &self,
        from: &Address,
        transfer: &Transfer,
        total_fee: &Wei,
    ) -> Result<(), Error> {
        let balance = self
            .balance(from.clone(), &AssetKind::Native, View::Pending)
            .await?;
        if let AssetKind::Erc20(_) = transfer.asset {
            if balance < *total_fee {
                return Err(Error::new(
                    ErrorKind::InsufficientFunds,
                    "Ethereum native balance is insufficient for the ERC-20 maximum fee",
                ));
            }
            return Ok(());
        }
        let required = transfer.value().checked_add(total_fee).ok_or_else(|| {
            Error::new(
                ErrorKind::FeeUnavailable,
                "Ethereum transfer value plus maximum fee overflowed U256",
            )
        })?;
        if balance < required {
            return Err(Error::new(
                ErrorKind::InsufficientFunds,
                "Ethereum native balance is insufficient for transfer value and maximum fee",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use futures_executor::block_on;

    use super::*;
    use crate::native::rpc::script::{CHAIN_ID, Reply, Script, Scripted};

    fn limits() -> FeeLimits {
        FeeLimits::new(
            1024,
            2_000,
            1_000_000,
            Wei::from_u128(1_000_000_000_000),
            Wei::from_u128(100_000_000_000),
            Wei::from_u128(1_000_000_000_000_000_000),
        )
        .unwrap()
    }

    fn tight() -> FeeLimits {
        FeeLimits::new(
            1024,
            0,
            10,
            Wei::from_u128(100),
            Wei::from_u128(10),
            Wei::from_u128(100),
        )
        .unwrap()
    }

    fn native(to: u8) -> Transfer {
        Transfer {
            asset: AssetKind::Native,
            to: Address::from_bytes([to; 20]),
            amount: Wei::from_u128(7),
        }
    }

    fn token() -> Transfer {
        Transfer {
            asset: AssetKind::Erc20(Address::from_bytes([0x33; 20])),
            to: Address::from_bytes([0x22; 20]),
            amount: Wei::from_u128(7),
        }
    }

    /// The sender of the ERC-20 fixtures.
    const FROM: Address = Address::from_bytes([0x11; 20]);

    fn abi_word(value: u8) -> Value {
        json!(format!("0x{}{value:02x}", "00".repeat(31)))
    }

    fn context(
        script: &Script,
        from: &Address,
        transfer: &Transfer,
        nonce: u64,
    ) -> Result<BuildContext, Error> {
        block_on(script.client().context(from, transfer, nonce, &limits()))
    }

    /// The five reads a funded native transfer needs, in order.
    fn native_steps() -> Vec<Reply> {
        let call = json!({"from": Address::from_bytes([1; 20]).to_string(),
            "to": Address::from_bytes([2; 20]).to_string(), "value": "0x7", "data": "0x"});
        vec![
            Reply::ok("eth_chainId", json!("0x7a69")),
            Reply::ok("eth_estimateGas", json!("0x2")).expecting(json!([call])),
            Reply::ok("eth_maxPriorityFeePerGas", json!("0x2")),
            Reply::ok("eth_getBlockByNumber", json!({"baseFeePerGas": "0x3"}))
                .expecting(json!(["latest", false])),
            Reply::ok("eth_getBalance", json!("0xffff"))
                .expecting(json!([Address::from_bytes([1; 20]).to_string(), "pending"])),
        ]
    }

    #[test]
    fn fee_failures_stop_before_later_observations_and_keep_exact_errors() {
        for (length, replacement, kind, message) in [
            (
                2,
                json!("0x0"),
                ErrorKind::RpcUnavailable,
                "Ethereum RPC eth_estimateGas returned an invalid response: \
                 estimated gas limit is zero",
            ),
            (
                2,
                json!("0xb"),
                ErrorKind::FeeUnavailable,
                "Ethereum estimated gas limit exceeds the configured ceiling",
            ),
            (
                3,
                json!("0xb"),
                ErrorKind::FeeUnavailable,
                "Ethereum priority fee exceeds the configured ceiling",
            ),
            (
                4,
                json!({}),
                ErrorKind::RpcUnavailable,
                "Ethereum RPC eth_getBlockByNumber returned an invalid response: \
                 latest block has no EIP-1559 baseFeePerGas",
            ),
            (
                4,
                json!({"baseFeePerGas": "0x32"}),
                ErrorKind::FeeUnavailable,
                "Ethereum max fee per gas exceeds the configured ceiling",
            ),
            (
                4,
                json!({"baseFeePerGas": "0x31"}),
                ErrorKind::FeeUnavailable,
                "Ethereum maximum transaction fee exceeds the configured ceiling",
            ),
        ] {
            let mut steps = native_steps();
            steps.truncate(length);
            let last = steps.pop().unwrap();
            let method = if length == 2 {
                "eth_estimateGas"
            } else if length == 3 {
                "eth_maxPriorityFeePerGas"
            } else {
                "eth_getBlockByNumber"
            };
            drop(last);
            steps.push(Reply::ok(method, replacement));
            let script = Script::node(steps);
            let error = block_on(script.client().context(
                &Address::from_bytes([1; 20]),
                &native(2),
                99,
                &tight(),
            ))
            .unwrap_err();
            assert_eq!(error.kind, kind);
            assert_eq!(error.message, message);
            script.exhausted();
        }
    }

    #[test]
    fn prepared_fees_preserve_the_reserved_nonce_without_a_nonce_lookup() {
        let script = Script::node(native_steps());
        assert_eq!(
            block_on(script.client().context(
                &Address::from_bytes([1; 20]),
                &native(2),
                u64::MAX,
                &tight()
            ))
            .unwrap(),
            BuildContext {
                chain_id: CHAIN_ID,
                from: Address::from_bytes([1; 20]),
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
                ErrorKind::RpcUnavailable,
                "Ethereum RPC eth_getBlockByNumber returned an invalid response: \
                 fee calculation overflowed U256",
            ),
            (
                format!("0x7{}", "f".repeat(63)),
                ErrorKind::FeeUnavailable,
                "Ethereum maximum transaction fee overflowed U256",
            ),
        ] {
            let mut steps = native_steps();
            steps.truncate(2);
            steps.push(Reply::ok("eth_maxPriorityFeePerGas", json!("0x1")));
            steps.push(Reply::ok(
                "eth_getBlockByNumber",
                json!({"baseFeePerGas": base_fee}),
            ));
            let script = Script::node(steps);
            let wide = FeeLimits::new(
                1024,
                0,
                10,
                Wei::from_atomic([255; 32]),
                Wei::from_u128(10),
                Wei::from_atomic([255; 32]),
            )
            .unwrap();
            let error = block_on(script.client().context(
                &Address::from_bytes([1; 20]),
                &native(2),
                99,
                &wide,
            ))
            .unwrap_err();
            assert_eq!(error.kind, kind);
            assert_eq!(error.message, message);
            script.exhausted();
        }
    }

    #[test]
    fn builds_checked_eip1559_context_for_native_transfer() {
        let script = Script::node(vec![
            Reply::ok("eth_chainId", json!("0x7a69")),
            Reply::ok("eth_estimateGas", json!("0x5208")),
            Reply::ok("eth_maxPriorityFeePerGas", json!("0x3b9aca00")),
            Reply::ok(
                "eth_getBlockByNumber",
                json!({"baseFeePerGas": "0x77359400"}),
            ),
            Reply::ok("eth_getBalance", json!("0xde0b6b3a7640000")),
        ]);
        let context = context(&script, &Address::from_bytes([0x0a; 20]), &native(0x0b), 4).unwrap();
        assert_eq!(context.chain_id, CHAIN_ID);
        assert_eq!(context.nonce, 4);
        assert_eq!(context.gas_limit, 25_200);
        assert_eq!(
            context.max_priority_fee_per_gas,
            Wei::from_u128(1_000_000_000)
        );
        assert_eq!(context.max_fee_per_gas, Wei::from_u128(5_000_000_000));
        let call = &script.requests()[1].1[0];
        assert_eq!(call["from"], json!(format!("0x{}", "0a".repeat(20))));
        assert_eq!(call["to"], json!(format!("0x{}", "0b".repeat(20))));
        assert_eq!(call["data"], json!("0x"));
        assert_eq!(call["value"], json!("0x7"));
    }

    #[test]
    fn simulates_typed_erc20_transfer_and_checks_both_balances() {
        let script = Script::node(vec![
            Reply::ok("eth_chainId", json!("0x7a69")),
            Reply::ok("eth_call", abi_word(8)),
            Reply::ok("eth_call", abi_word(1)),
            Reply::ok("eth_estimateGas", json!("0x5208")),
            Reply::ok("eth_maxPriorityFeePerGas", json!("0x3b9aca00")),
            Reply::ok(
                "eth_getBlockByNumber",
                json!({"baseFeePerGas": "0x77359400"}),
            ),
            Reply::ok("eth_getBalance", json!("0xde0b6b3a7640000")),
        ]);
        context(&script, &FROM, &token(), 4).expect("funded ERC-20 transfer must build");
        let requests = script.requests();
        let expected_data = format!(
            "0xa9059cbb{}{}{}07",
            "00".repeat(12),
            "22".repeat(20),
            "00".repeat(31)
        );
        assert_eq!(
            requests[2].1[0]["to"],
            json!(Address::from_bytes([0x33; 20]).to_string())
        );
        assert_eq!(requests[2].1[0]["value"], json!("0x0"));
        assert_eq!(requests[2].1[0]["data"], json!(expected_data));
        assert_eq!(requests[2].1[1], json!("pending"));
        assert_eq!(requests[3].1[0], requests[2].1[0]);
        assert_eq!(
            requests[1].1[0]["data"],
            json!(format!("0x70a08231{}{}", "00".repeat(12), "11".repeat(20)))
        );
    }

    #[test]
    fn erc20_simulation_rejects_false_empty_malformed_and_reverted_returns() {
        for (reply, expected) in [
            (Reply::ok("eth_call", abi_word(0)), "returned false"),
            (Reply::ok("eth_call", json!("0x")), "invalid length"),
            (Reply::ok("eth_call", abi_word(2)), "invalid ABI result"),
            (
                Reply::failure(
                    "eth_call",
                    -32_000,
                    "execution reverted: Bearer secret https://user:password@example.invalid",
                ),
                "-32000",
            ),
        ] {
            let script = Script::node(vec![
                Reply::ok("eth_chainId", json!("0x7a69")),
                Reply::ok("eth_call", abi_word(8)),
                reply,
            ]);
            let error = context(&script, &FROM, &token(), 0).unwrap_err();
            assert_eq!(error.kind, ErrorKind::Rejected);
            assert!(error.message.contains(expected), "{}", error.message);
            for secret in ["Bearer secret", "password", "example.invalid"] {
                assert!(!error.message.contains(secret));
            }
            script.exhausted();
        }
    }

    #[test]
    fn preflight_rejects_insufficient_funds_for_value_fees_and_tokens() {
        let no_gas = Script::node(vec![
            Reply::ok("eth_chainId", json!("0x7a69")),
            Reply::ok("eth_call", abi_word(8)),
            Reply::ok("eth_call", abi_word(1)),
            Reply::ok("eth_estimateGas", json!("0x5208")),
            Reply::ok("eth_maxPriorityFeePerGas", json!("0x3b9aca00")),
            Reply::ok(
                "eth_getBlockByNumber",
                json!({"baseFeePerGas": "0x77359400"}),
            ),
            Reply::ok("eth_getBalance", json!("0x0")),
        ]);
        let error = context(&no_gas, &FROM, &token(), 0).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InsufficientFunds);
        assert!(error.message.contains("ERC-20 maximum fee"));

        let no_tokens = Script::node(vec![
            Reply::ok("eth_chainId", json!("0x7a69")),
            Reply::ok("eth_call", abi_word(0)),
        ]);
        let error = context(&no_tokens, &FROM, &token(), 0).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InsufficientFunds);
        assert!(error.message.contains("ERC-20 balance is insufficient"));
        no_tokens.exhausted();

        let poor = Script::node(vec![
            Reply::ok("eth_chainId", json!("0x7a69")),
            Reply::ok("eth_estimateGas", json!("0x5208")),
            Reply::ok("eth_maxPriorityFeePerGas", json!("0x3b9aca00")),
            Reply::ok(
                "eth_getBlockByNumber",
                json!({"baseFeePerGas": "0x77359400"}),
            ),
            Reply::ok("eth_getBalance", json!("0x7")),
        ]);
        let error = context(&poor, &Address::from_bytes([1; 20]), &native(2), 0).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InsufficientFunds);
        assert!(error.message.contains("value and maximum fee"));
    }

    #[test]
    fn wrong_chain_id_and_revert_fail_before_later_queries() {
        let wrong = Script::node(vec![Reply::ok("eth_chainId", json!("0x1"))]);
        let error = context(&wrong, &Address::from_bytes([1; 20]), &native(2), 9).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Divergent);
        wrong.exhausted();

        let reverted = Script::node(vec![
            Reply::ok("eth_chainId", json!("0x7a69")),
            Reply::failure(
                "eth_estimateGas",
                -32_000,
                "execution reverted: Bearer secret https://user:password@example.invalid",
            ),
        ]);
        let error = context(&reverted, &Address::from_bytes([1; 20]), &native(2), 0).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Rejected);
        assert_eq!(
            error.message,
            "Ethereum gas estimation was rejected with code -32000"
        );
    }

    #[test]
    fn configured_input_and_fee_ceilings_fail_closed() {
        let no_calls = Script::node(Vec::new());
        let small_input = FeeLimits::new(
            32,
            2_000,
            1_000_000,
            Wei::from_u128(1_000_000_000_000),
            Wei::from_u128(100_000_000_000),
            Wei::from_u128(1_000_000_000_000_000_000),
        )
        .unwrap();
        let error =
            block_on(no_calls.client().context(&FROM, &token(), 0, &small_input)).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidTransaction);
        assert!(no_calls.requests().is_empty());

        let high_fee = Script::node(vec![
            Reply::ok("eth_chainId", json!("0x7a69")),
            Reply::ok("eth_estimateGas", json!("0x5208")),
            Reply::ok("eth_maxPriorityFeePerGas", json!("0x1")),
            Reply::ok(
                "eth_getBlockByNumber",
                json!({"baseFeePerGas": "0x100000000000"}),
            ),
        ]);
        let error = context(&high_fee, &Address::from_bytes([1; 20]), &native(2), 0).unwrap_err();
        assert_eq!(error.kind, ErrorKind::FeeUnavailable);
        assert!(error.message.contains("configured ceiling"));
    }
}
