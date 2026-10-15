//! [`Builder`]: one EIP-1559 transaction per Ethereum transfer.

use base::{
    Addressed as _, Asset, Future, Output, Outputs, Signer, TransactionError, Transactions,
    Transfer,
};

use crate::{
    Address, Client, FeeLimits, KeyPair,
    transaction::{Transaction, request},
};

/// Pays every transfer of one asset (ether, or the `contract` token), one
/// transaction each, from its own source or the default one: a sender's
/// first takes the node's pending nonce and each later one the next, so a
/// sender's batch lands in order or not past its first gap.
pub(crate) struct Builder {
    pub(crate) rpc: Client,
    pub(crate) limits: FeeLimits,
    pub(crate) asset: Asset,
    pub(crate) contract: Option<alloy_primitives::Address>,
    pub(crate) default: Option<Signer>,
    pub(crate) outputs: Outputs<Address>,
    /// Each output's own source, in output order.
    pub(crate) sources: Vec<Option<Signer>>,
}

impl base::Builder for Builder {
    fn with_default_source(&mut self, source: Signer) {
        self.default = Some(source);
    }

    fn transfer(&mut self, transfer: Transfer) -> Result<(), TransactionError> {
        let Transfer { to, amount, from } = transfer;
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
        self.sources.push(from);
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
            let network = self.rpc.network();
            // Each sender's key and next nonce, in order of first transfer.
            let mut senders: Vec<(KeyPair, u64)> = Vec::new();
            let mut order = Vec::with_capacity(self.sources.len());
            for source in &self.sources {
                let signer = source
                    .as_ref()
                    .or(self.default.as_ref())
                    .ok_or_else(|| TransactionError::failed("a transfer has no source"))?;
                let known = senders
                    .iter()
                    .position(|(key, _)| key.address() == &signer.address);
                let index = match known {
                    Some(index) => index,
                    None => {
                        let key = KeyPair::from_signer(signer, network)?;
                        let nonce = self
                            .rpc
                            .nonce(key.native_address())
                            .await
                            .map_err(TransactionError::unavailable)?;
                        senders.push((key, nonce));
                        senders.len() - 1
                    }
                };
                order.push(index);
            }
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
            for (output, index) in self.outputs.iter().zip(order) {
                let (key, nonce) = senders
                    .get_mut(index)
                    .ok_or_else(|| TransactionError::failed("unknown Ethereum sender"))?;
                let from = key.native_address();
                let mut native =
                    request(output, self.contract).map_err(TransactionError::failed)?;
                let gas = self.rpc.estimate(from, &native).await?;
                native.chain_id = network.chain_id;
                native.nonce = *nonce;
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
                transaction.sign(key).map_err(TransactionError::failed)?;
                signed.push(transaction.into());
                *nonce = nonce
                    .checked_add(1)
                    .ok_or_else(|| TransactionError::failed("Ethereum nonce is exhausted"))?;
            }
            Ok(Transactions(signed))
        })
    }
}
