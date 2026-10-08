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
