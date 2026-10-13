use base::{Address, Asset, BoxFuture, Builder, TransactionError};
use crypto::{Curve, SecretBytes};

/// The future every wallet capability returns: [`BoxFuture`] of an
/// [`anyhow::Result`] whose handled cases are [`crate::Error`], so the
/// traits stay `dyn`-safe.
pub type FutureResult<'a, T> = BoxFuture<'a, anyhow::Result<T>>;

/// One address with its key, its chain's builders and its broadcaster, all
/// injected by the factory that made it: what each chain implements. It
/// transacts every asset of its network that the factory knows, one asset
/// per builder.
pub trait Wallet: Send + Sync {
    /// The wallet's address.
    fn address(&self) -> &Address;

    /// A new transaction builder sending `asset` from this wallet: add
    /// transfers, then [`Builder::sign`].
    ///
    /// # Errors
    ///
    /// `Unsupported` for an asset this wallet does not send.
    fn transaction(&self, asset: &Asset) -> Result<Box<dyn Builder + '_>, TransactionError>;

    /// The key's curve and bytes, which is how a store files it.
    fn private_key(&self) -> (Curve, SecretBytes);
}
