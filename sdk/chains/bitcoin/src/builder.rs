//! [`Builder`]: one Bitcoin transfer in one transaction.

use std::sync::Arc;

use base::{
    Addressed as _, Asset, Decimal, Future, Input, InputStore, Inputs, Output, Reason, Signer,
    TransactionError, Transfer,
};

use crate::Address;
use crate::{
    KeyPair, fee::vsize, fee_cache::Cached, rpc::Client,
    transaction::Transaction as NativeTransaction,
};

/// Pays one transfer in one transaction: the estimator's cached fee rate,
/// the sender's coins largest first, change back to the default source,
/// else the sender, only when it is worth its own output (above dust after
/// paying for itself); a smaller leftover is fee.
pub(crate) struct Builder {
    pub(crate) rpc: Client,
    pub(crate) inputs: Arc<dyn InputStore>,
    /// The factory's estimator, whose cached rate prices the fee.
    pub(crate) fees: Arc<Cached>,
    pub(crate) default: Option<Signer>,
}

impl Builder {
    /// Every unspent coin of `key`, owned by its native address.
    async fn coins(&self, key: &KeyPair) -> Result<Inputs<Address>, TransactionError> {
        let found = self
            .inputs
            .find_unspent(std::slice::from_ref(key.address()))
            .await
            .map_err(TransactionError::unavailable)?;
        let coins: Vec<_> = found
            .into_iter()
            .filter(|input| input.address == *key.address())
            .map(|input| Input {
                coin: input.coin,
                address: key.native_address().clone(),
                asset: input.asset,
                amount: input.amount,
                height: input.height,
            })
            .collect();
        Ok(coins.into())
    }
}

impl base::Builder for Builder {
    fn with_default_source(&mut self, source: Signer) {
        self.default = Some(source);
    }

    fn sign<'b>(
        self: Box<Self>,
        transfer: Transfer,
    ) -> Future<'b, Result<base::Transaction, TransactionError>>
    where
        Self: 'b,
    {
        Box::pin(async move {
            let Transfer { to, amount, from } = transfer;
            let network = self.rpc.network();
            let address = Address::parse(&to.value, network).map_err(TransactionError::failed)?;
            if *address.address() != to {
                return Err(TransactionError::failed(
                    "not a canonical address of this Bitcoin network",
                ));
            }
            if amount.to_atomic_u64(0).ok().is_none_or(|value| value == 0) {
                return Err(TransactionError::failed(
                    "Bitcoin amount must be a positive number of satoshis",
                ));
            }
            let Some(signer) = from.as_ref().or(self.default.as_ref()) else {
                return Err(TransactionError::failed("the transfer has no source"));
            };
            let key = KeyPair::from_signer(signer, network)?;
            let owner = match &self.default {
                Some(signer) => KeyPair::from_signer(signer, network)?
                    .native_address()
                    .clone(),
                None => key.native_address().clone(),
            };
            let output = Output {
                address,
                asset: Asset::native(network.into()),
                amount,
            };
            let rate = self.fees.rate().await?;
            let inputs = self.coins(&key).await?;
            let (own, paid) = (owner.script_pubkey(), output.address.script_pubkey());
            let fee = |kept: &Inputs<Address>, change: bool| {
                let spent: Vec<_> = kept
                    .iter()
                    .map(|input| input.address.script_pubkey())
                    .collect();
                let outputs = std::iter::once(&paid).chain(change.then_some(&own));
                Decimal::from(rate.fee(vsize(
                    spent.iter().map(AsRef::as_ref),
                    outputs.map(AsRef::as_ref),
                )))
            };
            let requested = output.amount.clone();
            let chosen = inputs
                .largest_first()
                .filter(|kept, _| kept.total() < &requested + &fee(kept, false));
            if chosen.total() < &requested + &fee(&chosen, false) {
                return Err(TransactionError::failed(
                    "insufficient funds for the transfer and fee",
                )
                .with_reason(Reason::InsufficientFunds));
            }
            let dust = Decimal::from(own.minimal_non_dust().to_sat());
            let with_change = &requested + &fee(&chosen, true);
            let change = (chosen.total() >= &with_change + &dust).then(|| Output {
                address: owner.clone(),
                asset: Asset::native(network.into()),
                amount: chosen.total() - &with_change,
            });
            let mut transaction = NativeTransaction::from_inputs(
                &chosen,
                std::iter::once(&output).chain(&change),
                network,
            )
            .map_err(TransactionError::failed)?;
            transaction
                .sign(std::slice::from_ref(&key))
                .map_err(TransactionError::failed)?;
            Ok(transaction.into())
        })
    }
}
