//! [`Builder`]: one SOL or SPL token transfer on Solana.

use std::sync::Arc;

use base::{Addressed as _, Asset, Future, Output, Signer, TransactionError, Transfer};

use crate::{
    Address, Client, KeyPair, Token,
    fee_cache::Cached,
    transaction::{Transaction, message},
};

/// Pays one transfer of one asset (SOL, or the `token`) in one transaction,
/// from its own source or the default one, on a recent blockhash. A signed
/// transaction lands only until its blockhash expires (about a minute), at
/// its `expires_height`; then it fails to broadcast as `Expired`.
pub(crate) struct Builder {
    pub(crate) rpc: Client,
    /// The factory's estimator: its cached blockhash and signature price.
    pub(crate) fees: Arc<Cached>,
    pub(crate) asset: Asset,
    pub(crate) token: Option<Token>,
    pub(crate) default: Option<Signer>,
}

impl base::Builder for Builder {
    fn with_default_source(&mut self, source: Signer) {
        self.default = Some(source);
    }

    /// Refuses a recipient that is off-curve or the sender, and anything but
    /// a positive `u64` of atomic units.
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
                    "not a canonical address of this Solana network",
                ));
            }
            let Some(signer) = from.as_ref().or(self.default.as_ref()) else {
                return Err(TransactionError::failed("the transfer has no source"));
            };
            if !address.as_native().is_on_curve() || signer.address == to {
                return Err(TransactionError::failed(
                    "Solana recipient must be an on-curve address other than the sender",
                ));
            }
            if amount.to_atomic_u64(0).ok().is_none_or(|units| units == 0) {
                return Err(TransactionError::failed(
                    "Solana amount must be a positive 64-bit number of atomic units",
                ));
            }
            let key = KeyPair::from_signer(signer, network)?;
            let output = Output {
                address,
                asset: self.asset.clone(),
                amount,
            };
            let prices = self.fees.prices().await?;
            let sender = key.native_address();
            let message = message(sender, self.token.as_ref(), &output, &prices.blockhash)
                .map_err(TransactionError::failed)?;
            let fee = prices.fee(&message)?;
            let mut transaction = Transaction::pending(message, sender, &output, fee);
            transaction.sign(&key).map_err(TransactionError::failed)?;
            let mut signed = base::Transaction::from(transaction);
            signed.expires_height = Some(prices.expires_height);
            Ok(signed)
        })
    }
}
