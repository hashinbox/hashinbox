//! [`Builder`]: SOL or SPL token transfers of a Solana wallet.

use base::{
    Addressed as _, Asset, Decimal, Future, Output, Outputs, TransactionError, Transactions,
};

use crate::{
    Address, Client, KeyPair, Token,
    transaction::{Transaction, messages},
};

/// Pays every transfer of one asset (SOL, or the `token`) from one key: as
/// many per transaction as fit a packet, all on one recent blockhash. A
/// signed transaction lands only until its blockhash expires (about a
/// minute); then it fails to broadcast as `Expired`.
pub(crate) struct Builder<'a> {
    pub(crate) key: &'a KeyPair,
    pub(crate) rpc: &'a Client,
    pub(crate) asset: Asset,
    pub(crate) token: Option<Token>,
    pub(crate) outputs: Outputs<Address>,
}

impl base::Builder for Builder<'_> {
    /// Refuses an on-curve recipient other than the sender only, and anything
    /// but a positive `u64` of atomic units.
    fn transfer(&mut self, to: base::Address, amount: Decimal) -> Result<(), TransactionError> {
        let address =
            Address::parse(&to.value, self.rpc.network()).map_err(TransactionError::failed)?;
        if *address.address() != to {
            return Err(TransactionError::failed(
                "not a canonical address of this Solana network",
            ));
        }
        if !address.as_native().is_on_curve() || address == *self.key.native_address() {
            return Err(TransactionError::failed(
                "Solana recipient must be an on-curve address other than the sender",
            ));
        }
        if amount.to_atomic_u64(0).ok().is_none_or(|units| units == 0) {
            return Err(TransactionError::failed(
                "Solana amount must be a positive 64-bit number of atomic units",
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
            let blockhash = self
                .rpc
                .blockhash()
                .await
                .map_err(TransactionError::unavailable)?;
            let messages = messages(from, self.token.as_ref(), &self.outputs, &blockhash)
                .map_err(TransactionError::failed)?;
            let mut signed = Vec::with_capacity(messages.len());
            for (message, paid) in messages {
                let fee = self
                    .rpc
                    .fee(&message)
                    .await
                    .map_err(TransactionError::unavailable)?;
                let mut transaction = Transaction::pending(message, from, &self.outputs[paid], fee);
                transaction
                    .sign(self.key)
                    .map_err(TransactionError::failed)?;
                signed.push(transaction.into());
            }
            Ok(Transactions(signed))
        })
    }
}
