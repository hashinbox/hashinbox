//! [`Builder`]: every transfer of a Bitcoin builder in one transaction.

use std::sync::Arc;

use base::{
    Addressed as _, Asset, Decimal, Future, Input, InputStore, Inputs, Output, Outputs, Reason,
    Signer, TransactionError, Transactions, Transfer,
};

use crate::Address;
use crate::{
    FeeRate, KeyPair, fee::vsize, rpc::Client, transaction::Transaction as NativeTransaction,
};

/// Pays every transfer in one transaction: the node's fee rate (never below
/// the floor), the pooled coins of every sender largest first, each input
/// signed by its own key, change back to the default source, else the first
/// sender.
pub(crate) struct Builder {
    pub(crate) rpc: Client,
    pub(crate) inputs: Arc<dyn InputStore>,
    pub(crate) fee_target_blocks: u16,
    pub(crate) fee_floor: FeeRate,
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
            let estimate = self
                .rpc
                .fee_rate(self.fee_target_blocks)
                .await
                .map_err(TransactionError::unavailable)?;
            let rate = estimate.map_or(self.fee_floor, |rate| rate.max(self.fee_floor));
            let inputs = self.coins(&keys).await?;
            let own = owner.script_pubkey();
            // Priced with a change output, whether or not one is made.
            let scripts: Vec<_> = self
                .outputs
                .iter()
                .map(|output| output.address.script_pubkey())
                .chain([own.clone()])
                .collect();
            let fee = |kept: &Inputs<Address>| {
                let spent: Vec<_> = kept
                    .iter()
                    .map(|input| input.address.script_pubkey())
                    .collect();
                Decimal::from(rate.fee(vsize(
                    spent.iter().map(AsRef::as_ref),
                    scripts.iter().map(AsRef::as_ref),
                )))
            };
            let requested = self.outputs.total();
            let chosen = inputs
                .largest_first()
                .filter(|kept, _| kept.total() < &requested + &fee(kept));
            let needed = requested + fee(&chosen);
            if chosen.total() < needed {
                return Err(TransactionError::failed(
                    "insufficient funds for the transfers and fee",
                )
                .with_reason(Reason::InsufficientFunds));
            }
            let left = chosen.total() - needed;
            let change = (left >= Decimal::from(own.minimal_non_dust().to_sat())).then(|| Output {
                address: owner.clone(),
                asset: Asset::native(self.rpc.network().into()),
                amount: left,
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
