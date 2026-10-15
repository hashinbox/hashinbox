//! [`Builder`]: every transfer of a Bitcoin builder in one transaction.

use std::sync::Arc;

use base::{
    Addressed as _, Asset, Decimal, Future, Input, InputStore, Inputs, Output, Outputs, Reason,
    Signer, TransactionError, Transactions, Transfer,
};

use crate::Address;
use crate::{
    KeyPair,
    fee::{Fees, vsize},
    rpc::Client,
    transaction::Transaction as NativeTransaction,
};

/// Pays every transfer in one transaction: the estimator's cached fee rate,
/// the pooled coins of every sender largest first, each input
/// signed by its own key, change back to the default source, else the first
/// sender, only when it is worth its own output (above dust after paying for
/// itself); a smaller leftover is fee.
pub(crate) struct Builder {
    pub(crate) rpc: Client,
    pub(crate) inputs: Arc<dyn InputStore>,
    /// The factory's estimator, whose cached rate prices the fee.
    pub(crate) fees: Arc<Fees>,
    pub(crate) default: Option<Signer>,
    pub(crate) outputs: Outputs<Address>,
    /// Each output's own source, in output order.
    pub(crate) sources: Vec<Option<Signer>>,
}

impl Builder {
    /// The distinct sender keys, in transfer order, and the change owner.
    fn keys(&self) -> Result<(Vec<KeyPair>, Address), TransactionError> {
        let network = self.rpc.network();
        let mut keys: Vec<KeyPair> = Vec::new();
        for source in &self.sources {
            let signer = source
                .as_ref()
                .or(self.default.as_ref())
                .ok_or_else(|| TransactionError::failed("a transfer has no source"))?;
            if keys.iter().all(|key| key.address() != &signer.address) {
                keys.push(KeyPair::from_signer(signer, network)?);
            }
        }
        let change = match &self.default {
            Some(signer) => KeyPair::from_signer(signer, network)?
                .native_address()
                .clone(),
            None => keys
                .first()
                .ok_or_else(|| TransactionError::failed("no transfers to sign"))?
                .native_address()
                .clone(),
        };
        Ok((keys, change))
    }

    /// Every unspent coin of every sender, owned by its native address.
    async fn coins(&self, keys: &[KeyPair]) -> Result<Inputs<Address>, TransactionError> {
        let addresses: Vec<base::Address> = keys.iter().map(|key| key.address().clone()).collect();
        let found = self
            .inputs
            .find_unspent(&addresses)
            .await
            .map_err(TransactionError::unavailable)?;
        let coins: Vec<_> = found
            .into_iter()
            .filter_map(|input| {
                let key = keys.iter().find(|key| *key.address() == input.address)?;
                Some(Input {
                    coin: input.coin,
                    address: key.native_address().clone(),
                    asset: input.asset,
                    amount: input.amount,
                    height: input.height,
                })
            })
            .collect();
        Ok(coins.into())
    }
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
                "not a canonical address of this Bitcoin network",
            ));
        }
        if amount.to_atomic_u64(0).ok().is_none_or(|value| value == 0) {
            return Err(TransactionError::failed(
                "Bitcoin amount must be a positive number of satoshis",
            ));
        }
        self.outputs.push(Output {
            address,
            asset: Asset::native(self.rpc.network().into()),
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
            let (keys, owner) = self.keys()?;
            let rate = self.fees.rate().await?;
            let inputs = self.coins(&keys).await?;
            let own = owner.script_pubkey();
            let paid: Vec<_> = self
                .outputs
                .iter()
                .map(|output| output.address.script_pubkey())
                .collect();
            let fee = |kept: &Inputs<Address>, change: bool| {
                let spent: Vec<_> = kept
                    .iter()
                    .map(|input| input.address.script_pubkey())
                    .collect();
                let outputs = paid.iter().chain(change.then_some(&own));
                Decimal::from(rate.fee(vsize(
                    spent.iter().map(AsRef::as_ref),
                    outputs.map(AsRef::as_ref),
                )))
            };
            let requested = self.outputs.total();
            let chosen = inputs
                .largest_first()
                .filter(|kept, _| kept.total() < &requested + &fee(kept, false));
            if chosen.total() < &requested + &fee(&chosen, false) {
                return Err(TransactionError::failed(
                    "insufficient funds for the transfers and fee",
                )
                .with_reason(Reason::InsufficientFunds));
            }
            let dust = Decimal::from(own.minimal_non_dust().to_sat());
            let with_change = &requested + &fee(&chosen, true);
            let change = (chosen.total() >= &with_change + &dust).then(|| Output {
                address: owner.clone(),
                asset: Asset::native(self.rpc.network().into()),
                amount: chosen.total() - &with_change,
            });
            let mut transaction = NativeTransaction::from_inputs(
                &chosen,
                self.outputs.iter().chain(&change),
                self.rpc.network(),
            )
            .map_err(TransactionError::failed)?;
            transaction.sign(&keys).map_err(TransactionError::failed)?;
            Ok(Transactions(vec![transaction.into()]))
        })
    }
}
