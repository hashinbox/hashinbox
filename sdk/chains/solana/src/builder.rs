//! [`Builder`]: SOL or SPL token transfers on Solana.

use std::sync::Arc;

use base::{
    Addressed as _, Asset, Future, Output, Outputs, Signer, TransactionError, Transactions,
    Transfer,
};

use crate::{
    Address, Client, KeyPair, Token,
    fee::Estimator,
    transaction::{Transaction, messages},
};

/// Pays every transfer of one asset (SOL, or the `token`), each from its own
/// source or the default one: per sender, in order of first transfer, as
/// many per transaction as fit a packet, all on one recent blockhash. A
/// signed transaction lands only until its blockhash expires (about a
/// minute); then it fails to broadcast as `Expired`.
pub(crate) struct Builder {
    pub(crate) rpc: Client,
    /// The factory's estimator: its cached blockhash and signature price.
    pub(crate) fees: Arc<Estimator>,
    pub(crate) asset: Asset,
    pub(crate) token: Option<Token>,
    pub(crate) default: Option<Signer>,
    /// Each transfer with its own source, in order.
    pub(crate) transfers: Vec<(Output<Address>, Option<Signer>)>,
}

impl base::Builder for Builder {
    fn with_default_source(&mut self, source: Signer) {
        self.default = Some(source);
    }

    /// Refuses an on-curve recipient other than the sender (when known) only,
    /// and anything but a positive `u64` of atomic units.
    fn transfer(&mut self, transfer: Transfer) -> Result<(), TransactionError> {
        let Transfer { to, amount, from } = transfer;
        let address =
            Address::parse(&to.value, self.rpc.network()).map_err(TransactionError::failed)?;
        if *address.address() != to {
            return Err(TransactionError::failed(
                "not a canonical address of this Solana network",
            ));
        }
        let sender = from.as_ref().or(self.default.as_ref());
        if !address.as_native().is_on_curve() || sender.is_some_and(|sender| sender.address == to) {
            return Err(TransactionError::failed(
                "Solana recipient must be an on-curve address other than the sender",
            ));
        }
        if amount.to_atomic_u64(0).ok().is_none_or(|units| units == 0) {
            return Err(TransactionError::failed(
                "Solana amount must be a positive 64-bit number of atomic units",
            ));
        }
        let output = Output {
            address,
            asset: self.asset.clone(),
            amount,
        };
        self.transfers.push((output, from));
        Ok(())
    }

    fn sign<'b>(self: Box<Self>) -> Future<'b, Result<Transactions, TransactionError>>
    where
        Self: 'b,
    {
        Box::pin(async move {
            if self.transfers.is_empty() {
                return Err(TransactionError::failed("no transfers to sign"));
            }
            let network = self.rpc.network();
            // Each sender's key and outputs, in order of first transfer.
            let mut senders: Vec<(KeyPair, Outputs<Address>)> = Vec::new();
            for (output, source) in &self.transfers {
                let signer = source
                    .as_ref()
                    .or(self.default.as_ref())
                    .ok_or_else(|| TransactionError::failed("a transfer has no source"))?;
                if output.address.address() == &signer.address {
                    return Err(TransactionError::failed(
                        "Solana recipient must be other than the sender",
                    ));
                }
                let known = senders
                    .iter()
                    .position(|(key, _)| key.address() == &signer.address);
                let index = match known {
                    Some(index) => index,
                    None => {
                        senders.push((KeyPair::from_signer(signer, network)?, Outputs::default()));
                        senders.len() - 1
                    }
                };
                if let Some((_, outputs)) = senders.get_mut(index) {
                    outputs.push(output.clone());
                }
            }
            let prices = self.fees.prices().await?;
            let mut signed = Vec::new();
            for (key, outputs) in &senders {
                let from = key.native_address();
                let messages = messages(from, self.token.as_ref(), outputs, &prices.blockhash)
                    .map_err(TransactionError::failed)?;
                for (message, paid) in messages {
                    let fee = prices.fee(&message)?;
                    let mut transaction = Transaction::pending(message, from, &outputs[paid], fee);
                    transaction.sign(key).map_err(TransactionError::failed)?;
                    signed.push(transaction.into());
                }
            }
            Ok(Transactions(signed))
        })
    }
}
