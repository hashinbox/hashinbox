//! [`Builder`]: one EIP-1559 transaction per transfer of an Ethereum wallet.

use base::{
    Addressed as _, Asset, Decimal, Future, Output, Outputs, TransactionError, Transactions,
};

use crate::{
    Address, Client, FeeLimits, KeyPair,
    transaction::{Transaction, request},
};

/// Pays every transfer of one asset (ether, or the `contract` token) from one
/// key, one transaction each: the first takes the node's pending nonce and
/// each later one the next, so a batch lands in order or not past its first
/// gap.
pub(crate) struct Builder<'a> {
    pub(crate) key: &'a KeyPair,
    pub(crate) rpc: &'a Client,
    pub(crate) limits: FeeLimits,
    pub(crate) asset: Asset,
    pub(crate) contract: Option<alloy_primitives::Address>,
    pub(crate) outputs: Outputs<Address>,
}

impl base::Builder for Builder<'_> {
    fn transfer(&mut self, to: base::Address, amount: Decimal) -> Result<(), TransactionError> {
        let address =
            Address::parse(&to.value, self.rpc.network()).map_err(TransactionError::failed)?;
        if *address.address() != to {
            return Err(TransactionError::failed(
                "not a canonical address of this Ethereum network",
            ));
        }
        if amount
            .to_atomic_be_bytes::<32>(0)
            .ok()
            .is_none_or(|word| word == [0; 32])
        {
            return Err(TransactionError::failed(
                "Ethereum amount must be a positive 256-bit number of atomic units",
            ));
        }
        self.outputs.push(Output {
            address,
            asset: self.asset.clone(),
            amount,
        });
        Ok(())
    }

    fn sign<'b>(self: Box<Self>) -> Future<'b, Result<Transactions, TransactionError>>
    where
        Self: 'b,
    {
        Box::pin(async move {
            if self.outputs.is_empty() {
                return Err(TransactionError::failed("no transfers to sign"));
            }
            let from = self.key.native_address();
            let mut nonce = self
                .rpc
                .nonce(from)
                .await
                .map_err(TransactionError::unavailable)?;
            let (base_fee, priority_fee) = self
                .rpc
                .fees()
                .await
                .map_err(TransactionError::unavailable)?;
            let fees = self
                .limits
                .fees(base_fee, priority_fee)
                .map_err(TransactionError::failed)?;
            let mut signed = Vec::with_capacity(self.outputs.len());
            for output in self.outputs.iter() {
                let mut native =
                    request(output, self.contract).map_err(TransactionError::failed)?;
                let gas = self.rpc.estimate(from, &native).await?;
                native.chain_id = self.rpc.network().chain_id;
                native.nonce = nonce;
                native.gas_limit = self
                    .limits
                    .gas_limit(gas)
                    .map_err(TransactionError::failed)?;
                native.max_fee_per_gas = fees.max_fee_per_gas;
                native.max_priority_fee_per_gas = fees.max_priority_fee_per_gas;
                self.limits
                    .total(native.gas_limit, fees)
                    .map_err(TransactionError::failed)?;
                let mut transaction = Transaction::pending(native, from, output);
                transaction
                    .sign(self.key)
                    .map_err(TransactionError::failed)?;
                signed.push(transaction.into());
                nonce = nonce
                    .checked_add(1)
                    .ok_or_else(|| TransactionError::failed("Ethereum nonce is exhausted"))?;
            }
            Ok(Transactions(signed))
        })
    }
}
