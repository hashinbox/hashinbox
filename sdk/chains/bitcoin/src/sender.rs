//! `wallets::Sender`: one batch of transfers as one transaction through the
//! factory's outbox, each sender paying its own transfers, change, and share
//! of the fee; and where its inputs come from ([`UtxoSource`], [`Spendable`]).

mod dispatch;
mod outbox;
mod recovery;
mod reservation;
mod spendable;

mod utxo_source;

use std::{any::Any, sync::Arc};

use wallets::{
    Author, Authorship, SendError, SendFuture, Sender, Transfer, WalletError, WalletErrorKind,
};

pub(crate) use outbox::{Outbox, SendRequest};
pub use spendable::Spendable;
pub use utxo_source::UtxoSource;

use crate::WalletConfig;
use crate::native::{Address, Satoshi, transaction};
use crate::wallet::Wallet;

/// One factory's `wallets::Sender` and `wallets::Authorship`: its
/// configuration and the outbox its wallets share. It resolves every transfer
/// before any chain call, then hands the batch to the outbox, which reserves
/// the outputs it spends.
pub(crate) struct FactorySender {
    config: WalletConfig,
    outbox: Arc<Outbox>,
}

impl FactorySender {
    pub(crate) const fn new(config: WalletConfig, outbox: Arc<Outbox>) -> Self {
        Self { config, outbox }
    }

    /// The send request of one authored transfer; the wallet signs it.
    fn request(&self, transfer: Transfer) -> Result<SendRequest, WalletError> {
        let from = Address::try_from(transfer.wallet.address())?;
        let to = self.config.destination(&transfer.to)?;
        match transfer.amount {
            wallets::Amount::Exact(amount) => {
                let amount = Satoshi::from_decimal(&amount).map_err(|error| {
                    WalletError::new(WalletErrorKind::InvalidAmount, error.message)
                })?;
                Ok(SendRequest::new(
                    transfer.wallet,
                    from,
                    transaction::Transfer::new(to, amount),
                ))
            }
            wallets::Amount::All => Ok(SendRequest::sweep(transfer.wallet, from, to)),
        }
    }

    async fn execute(
        &self,
        transfers: Vec<Transfer>,
    ) -> Result<Vec<base::TransactionId>, SendError> {
        SendError::check_count(transfers.len())?;
        SendError::check_selection(&transfers)?;
        let items = transfers
            .into_iter()
            .enumerate()
            .map(|(index, transfer)| {
                self.request(transfer)
                    .map_err(|error| SendError::item(index, Vec::new(), error))
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.outbox.send(items).await
    }
}

impl Sender for FactorySender {
    fn send<'a>(&'a self, transfers: Vec<Transfer>) -> SendFuture<'a> {
        Box::pin(self.execute(transfers))
    }
}

impl Authorship for FactorySender {
    /// The wallet's own authoring capability, recovered by the one type that
    /// makes it. Membership is `WalletFactory::validate`'s answer, which
    /// `wallets::Wallets::author` has already taken; this repeats only the
    /// two facts a send pipeline needs — one configuration, one outbox.
    fn author(&self, wallet: &Arc<dyn wallets::Wallet>) -> Result<Arc<dyn Author>, WalletError> {
        let wallet = (wallet.as_ref() as &dyn Any)
            .downcast_ref::<Wallet>()
            .filter(|wallet| {
                wallet.config == self.config && Arc::ptr_eq(&wallet.outbox, &self.outbox)
            })
            .ok_or_else(|| {
                WalletError::new(
                    WalletErrorKind::Unsupported,
                    "wallet was not created by this asset's factory",
                )
            })?;
        Ok(Arc::new(wallet.clone()))
    }
}
