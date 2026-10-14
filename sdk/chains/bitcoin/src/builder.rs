//! [`Builder`]: every transfer of a Bitcoin wallet in one transaction.

use base::{
    Addressed as _, Asset, Decimal, Future, InputStore, Networked as _, Output, Outputs, Reason,
    TransactionError, Transactions,
};

use crate::Address;
use crate::{
    FeeRate, KeyPair, fee::vsize, rpc::Client, transaction::Transaction as NativeTransaction,
};

/// Pays every transfer from one key in one transaction: the node's fee rate
/// (never below the floor), the store's coins largest first, change back to
/// the key's address.
pub(crate) struct Builder<'a> {
    pub(crate) key: &'a KeyPair,
    pub(crate) rpc: &'a Client,
    pub(crate) inputs: &'a dyn InputStore,
    pub(crate) fee_target_blocks: u16,
    pub(crate) fee_floor: FeeRate,
    pub(crate) outputs: Outputs<Address>,
}

impl base::Builder for Builder<'_> {
    fn transfer(&mut self, to: base::Address, amount: Decimal) -> Result<(), TransactionError> {
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
            asset: Asset::native(self.key.network().clone()),
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
            let estimate = self
                .rpc
                .fee_rate(self.fee_target_blocks)
                .await
                .map_err(TransactionError::unavailable)?;
            let rate = estimate.map_or(self.fee_floor, |rate| rate.max(self.fee_floor));
            let inputs = self
                .inputs
                .find_unspent_for_address(self.key.address())
                .await
                .map_err(TransactionError::unavailable)?;
            let owner = self.key.native_address();
            let own = owner.script_pubkey();
            // Priced with a change output, whether or not one is made.
            let scripts: Vec<_> = self
                .outputs
                .iter()
                .map(|output| output.address.script_pubkey())
                .chain([own.clone()])
                .collect();
            let fee = |count: usize| {
                Decimal::from(rate.fee(vsize(&own, count, scripts.iter().map(AsRef::as_ref))))
            };
            let requested = self.outputs.total();
            let chosen = inputs
                .largest_first()
                .filter(|kept, _| kept.total() < &requested + &fee(kept.len()));
            let needed = requested + fee(chosen.len());
            if chosen.total() < needed {
                return Err(TransactionError::failed(
                    "insufficient funds for the transfers and fee",
                )
                .with_reason(Reason::InsufficientFunds));
            }
            let left = chosen.total() - needed;
            let change = (left >= Decimal::from(own.minimal_non_dust().to_sat())).then(|| Output {
                address: owner.clone(),
                asset: Asset::native(self.key.network().clone()),
                amount: left,
            });
            let mut transaction = NativeTransaction::from_inputs(
                &chosen,
                owner,
                self.outputs.iter().chain(&change),
                self.rpc.network(),
            )
            .map_err(TransactionError::failed)?;
            transaction
                .sign(self.key)
                .map_err(TransactionError::failed)?;
            Ok(Transactions(vec![transaction.into()]))
        })
    }
}
