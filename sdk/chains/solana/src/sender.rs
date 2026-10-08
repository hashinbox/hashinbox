//! `wallets::Sender`: one batch of native SOL transfers through the
//! factory's outbox, one transaction per transfer, submitted in order and
//! stopped at the first failure; and how an unknown outcome learns whether
//! it landed ([`Presence`], [`IndexPresence`]).

mod dispatch;
mod outbox;
mod presence;
mod recovery;
mod registration;
mod reservation;

use std::{any::Any, sync::Arc};

use wallets::{
    Author, Authorship, SendError, SendFuture, Sender, Transfer, WalletError, WalletErrorKind,
};

pub(crate) use outbox::{Outbox, SendRequest};
pub use presence::{IndexPresence, Landing, Presence};
pub(crate) use recovery::{CommitSignal, Recovery};
pub(crate) use registration::Spawner;
pub(crate) use reservation::Reservations;

use base::Decimal;

use crate::WalletConfig;
use crate::native::{Address, Lamport, transaction};
use crate::wallet::Wallet;

/// One factory's `wallets::Sender` and `wallets::Authorship`: its
/// configuration and the outbox it shares with its siblings. It resolves
/// every transfer before any chain call, then hands the batch to the outbox,
/// which reserves each sender for the whole batch.
///
/// Only a native SOL factory builds one: an SPL factory declares no
/// `wallets::Authorship`, so a token batch does not exist to refuse.
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
        let to = Address::try_from(&transfer.to)?;
        let amount = match transfer.amount {
            wallets::Amount::Exact(amount) => amount,
            wallets::Amount::All => return SendRequest::sweep(transfer.wallet, from, to),
        };
        let native = native_transfer(&from, to, &amount, self.config.decimals)?;
        Ok(SendRequest::new(transfer.wallet, from, native))
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

/// The native SOL transfer of `amount` from `from` to `to`, read in
/// `decimals`.
///
/// The scale is an argument because the amount type takes one: SOL's nine
/// belongs to the caller's `WalletConfig`, not to this function.
///
/// # Errors
///
/// `Unsupported` for an off-curve destination, `AddressMismatch` when `to`
/// is the sender, and `InvalidAmount` unless `amount` is a positive number of
/// atomic units at that scale.
pub(crate) fn native_transfer(
    from: &Address,
    to: Address,
    amount: &Decimal,
    decimals: u32,
) -> Result<transaction::Transfer, WalletError> {
    destination(from, &to)?;
    let amount = Lamport::from_decimal(amount, decimals).map_err(|_| {
        WalletError::new(
            WalletErrorKind::InvalidAmount,
            "native SOL amount is invalid",
        )
    })?;
    Ok(transaction::Transfer::new(to, amount)?)
}

/// The native send boundary, shared by exact transfers and unresolved sweeps.
fn destination(from: &Address, to: &Address) -> Result<(), WalletError> {
    if !to.is_on_curve() {
        return Err(WalletError::new(
            WalletErrorKind::Unsupported,
            "unsupported Solana native destination",
        ));
    }
    if to == from {
        return Err(WalletError::new(
            WalletErrorKind::AddressMismatch,
            "Solana source and destination must differ",
        ));
    }
    Ok(())
}
