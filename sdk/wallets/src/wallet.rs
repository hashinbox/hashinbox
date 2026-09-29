use std::any::Any;

use base::{BoxFuture, Broadcaster, Decimal, Signer, TransactionBuilder, TransactionId};
use indexing::{Address, Holder};

use crate::WalletError;

/// The future every wallet capability returns: [`BoxFuture`] of this crate's
/// [`WalletError`], so the traits stay `dyn`-safe.
pub type FutureResult<'a, T> = BoxFuture<'a, Result<T, WalletError>>;

/// Build/restore/broadcast access to a wallet's transactions.
///
/// A supertrait of [`Author`], not of [`Wallet`]: an asset this system
/// receives and reports but never spends has no builder to reach.
pub trait TransactionFactory: Send + Sync {
    /// A new builder signing with this wallet.
    fn transaction(&self) -> Box<dyn TransactionBuilder>;

    /// Rebuilds a builder from a draft's snapshot, reinjecting this wallet's
    /// signer and RPC. Rejects a snapshot of another wallet, network, or
    /// asset.
    fn resume(
        &self,
        snapshot: &base::TransactionSnapshot,
    ) -> Result<Box<dyn TransactionBuilder>, base::TransactionError>;

    /// Submits transactions this wallet prepared.
    fn broadcaster(&self) -> &dyn Broadcaster;
}

/// Submits one transfer through the concrete wallet's send pipeline.
///
/// `destination` is an [`Address`] of the wallet's own network, which
/// [`crate::Wallets::parse`] is the way to obtain: it only ever produces
/// addresses of that asset's network.
pub trait SingleSender: Send + Sync {
    /// Sends `amount` (display units) and returns the transaction id.
    fn send<'a>(&'a self, destination: Address, amount: Decimal)
    -> FutureResult<'a, TransactionId>;
}

/// What every wallet is, whatever its asset: an address holding one asset,
/// and a key that signs.
///
/// It is an [`indexing::Holder`], so `index.watch(&id, &*wallet, start)` and
/// every index read take the wallet itself — balance and history need no key
/// and no send pipeline, which is why a watch-only address answers them the
/// same way.
///
/// Spending is **not** here. A wallet whose asset this system also spends is
/// an [`Author`], and [`crate::Wallets::author`] is the only way to obtain
/// one. A holder of `Arc<dyn Wallet>` therefore cannot author a payment at
/// all — the attempt does not compile:
///
/// ```compile_fail,E0599
/// use std::sync::Arc;
///
/// fn draft(wallet: &Arc<dyn wallets::Wallet>) {
///     wallet.transaction();
/// }
/// ```
///
/// ```compile_fail,E0599
/// use std::sync::Arc;
///
/// async fn pay(wallet: &Arc<dyn wallets::Wallet>, to: base::Address, amount: base::Decimal) {
///     wallet.send(to, amount).await.expect("no such capability");
/// }
/// ```
pub trait Wallet: Holder + Signer + Send + Sync + Any {}

impl<T: Holder + Signer + Send + Sync + Any> Wallet for T {}

/// A [`Wallet`] whose asset this system also spends: it sends one transfer
/// ([`SingleSender`]) and builds, restores and broadcasts its own
/// transactions ([`TransactionFactory`]), reachable from the trait object
/// without downcasting to a concrete chain wallet.
///
/// Only [`crate::Wallets::author`] hands one out, and only for an asset whose
/// factory declared a [`crate::Authorship`]. An asset the system receives and
/// reports — an SPL holding, say — declares none, so no value of this type
/// can exist for it and no chain needs a runtime gate saying so.
pub trait Author: Wallet + SingleSender + TransactionFactory {}

impl<T: Wallet + SingleSender + TransactionFactory> Author for T {}
