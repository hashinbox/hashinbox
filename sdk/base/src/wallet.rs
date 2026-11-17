//! Wallets: the [`Wallet`] trait a chain implements, its [`Factory`], and
//! [`Factories`], every configured network's factory and broadcaster.
//! Handled failures are [`Error`]s inside an [`anyhow::Error`].

use std::{collections::BTreeMap, error, fmt, sync::Arc};

use anyhow::Result;
use crypto::{Curve, SecretBytes};

use crate::{
    Address, Addressed, Asset, Broadcaster, Builder, FeeEstimator, Future, Network, Networked,
    TransactionError,
};

/// One address with its key, its chain's builders and its broadcaster, all
/// injected by the factory that made it: what each chain implements. It
/// transacts every asset of its network that the factory knows, one asset
/// per builder.
pub trait Wallet: Addressed + Networked + Send + Sync {
    /// A new transaction builder sending `asset`, with this wallet as its
    /// default source: the factory's builder plus
    /// [`Builder::with_default_source`]; [`Builder::sign`] pays its one
    /// transfer.
    ///
    /// # Errors
    ///
    /// `Unsupported` for an asset this wallet does not send.
    fn transaction(&self, asset: &Asset) -> Result<Box<dyn Builder>, TransactionError>;

    /// The key's curve and bytes, which is how a store files it.
    fn private_key(&self) -> (Curve, SecretBytes);
}

/// Makes the wallets of one network; a chain's `Factory` implements it,
/// and [`Factories::set`] registers it at the composition root.
pub trait Factory: Send + Sync {
    /// A new key in the chain's import format, on its own curve:
    /// [`Factory::from_pk`] builds the wallet from it.
    ///
    /// # Errors
    ///
    /// The chain's key generation failure.
    fn generate(&self) -> Future<'_, anyhow::Result<SecretBytes>>;

    /// The wallet of `secret`: 32 bytes of key material, which every
    /// supported chain writes as 64 hexadecimal characters and
    /// [`crypto::SecretBytes::from_hex`] decodes chain-neutrally.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidSecret`] when the bytes are not a key
    /// of this chain's curve.
    #[allow(
        clippy::wrong_self_convention,
        reason = "constructs a wallet from a private key; the factory is the receiver"
    )]
    fn from_pk(&self, secret: SecretBytes) -> Future<'_, anyhow::Result<Arc<dyn Wallet>>>;

    /// The wallet of `text`, a raw private key in the chain's own import
    /// format (Ethereum hex, Solana Base58 secret or keypair, Bitcoin WIF),
    /// built as [`Factory::from_pk`] builds it.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidSecret`] when `text` is not a key in that
    /// format. The message quotes nothing back.
    #[allow(
        clippy::wrong_self_convention,
        reason = "constructs a wallet from a private key's text; the factory is the receiver"
    )]
    fn from_pk_text(&self, text: &str) -> Future<'_, anyhow::Result<Arc<dyn Wallet>>>;

    /// The canonical [`Address`] of `text` in this factory's network, without
    /// I/O or a key; accepts every textual form the chain defines.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidAddress`] when `text` is not an
    /// address of this network.
    fn parse_address(&self, text: &str) -> anyhow::Result<Address>;

    /// A new transaction builder sending `asset`, with no default source:
    /// its transfer names its own `from`, or a default is set.
    ///
    /// # Errors
    ///
    /// `Unsupported` for an asset this network's wallets do not send.
    fn transaction(&self, asset: &Asset) -> Result<Box<dyn Builder>, TransactionError>;

    /// The network's fee estimator: the one instance every builder this
    /// factory (and its wallets) makes prices its transactions with.
    fn fees(&self) -> Arc<dyn FeeEstimator>;
}

/// A boxed factory is a factory, so a composition root can hold the
/// factories of several chains as `Box<dyn Factory>`.
impl<F: Factory + ?Sized> Factory for Box<F> {
    fn generate(&self) -> Future<'_, anyhow::Result<SecretBytes>> {
        (**self).generate()
    }

    fn from_pk(&self, secret: SecretBytes) -> Future<'_, anyhow::Result<Arc<dyn Wallet>>> {
        (**self).from_pk(secret)
    }

    fn from_pk_text(&self, text: &str) -> Future<'_, anyhow::Result<Arc<dyn Wallet>>> {
        (**self).from_pk_text(text)
    }

    fn parse_address(&self, text: &str) -> anyhow::Result<Address> {
        (**self).parse_address(text)
    }

    fn transaction(&self, asset: &Asset) -> Result<Box<dyn Builder>, TransactionError> {
        (**self).transaction(asset)
    }

    fn fees(&self) -> Arc<dyn FeeEstimator> {
        (**self).fees()
    }
}

/// One network's factory and the broadcaster it hands its wallets.
#[derive(Clone)]
pub struct FactoryBroadcaster {
    /// The one network its wallets live in.
    pub network: Network,
    /// Makes the network's wallets and parses its addresses.
    pub factory: Arc<dyn Factory>,
    /// Sends exact signed bytes to the network's nodes.
    pub broadcaster: Arc<dyn Broadcaster>,
}

impl crate::Networked for FactoryBroadcaster {
    fn network(&self) -> &Network {
        &self.network
    }
}

/// The wallet factory and broadcaster of every configured network: they make
/// wallets and parse addresses. Live wallets are loaded from a the application's store.
#[derive(Clone, Default)]
pub struct Factories {
    networks: BTreeMap<Network, FactoryBroadcaster>,
}

impl Factories {
    /// An empty register.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `entry` under its network, replacing whatever that network held.
    pub fn set(&mut self, entry: FactoryBroadcaster) {
        self.networks.insert(entry.network.clone(), entry);
    }

    /// The factory and broadcaster of `network`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Unsupported`] for a network without a factory.
    pub fn get(&self, network: &Network) -> Result<&FactoryBroadcaster> {
        self.networks.get(network).ok_or_else(|| {
            Error::new(ErrorKind::Unsupported, "no wallet factory for this network").into()
        })
    }

    /// The wallet of `network` that `secret` builds, kept by nothing.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Unsupported`] for a network without a factory;
    /// [`ErrorKind::InvalidSecret`] when the bytes are not a key of
    /// this chain's curve.
    pub fn from_pk<'a>(
        &'a self,
        network: &Network,
        secret: SecretBytes,
    ) -> Future<'a, anyhow::Result<Arc<dyn Wallet>>> {
        let entry = self.get(network).cloned();
        Box::pin(async move { entry?.factory.from_pk(secret).await })
    }

    /// A new key of `network` and its wallet, kept by nothing: store the key
    /// wherever the application keeps keys.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Unsupported`] for a network without a factory;
    /// The chain's key generation failure.
    pub fn generate<'a>(
        &'a self,
        network: &Network,
    ) -> Future<'a, anyhow::Result<(Arc<dyn Wallet>, SecretBytes)>> {
        let entry = self.get(network).cloned();
        Box::pin(async move {
            let factory = entry?.factory;
            let secret = factory.generate().await?;
            let wallet = factory.from_pk(SecretBytes::new(secret.as_bytes())).await?;
            Ok((wallet, secret))
        })
    }

    /// Parses user-supplied address text in `network` into its canonical
    /// [`Address`]: a destination, or an address to watch.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Unsupported`] for a network without a factory;
    /// [`ErrorKind::InvalidAddress`] from the factory's parse.
    pub fn parse(&self, network: &Network, text: &str) -> Result<Address> {
        self.get(network)?.factory.parse_address(text)
    }
}

/// The wallet failures a caller handles differently, carried inside an
/// [`anyhow::Error`] and found with `downcast_ref::<Error>()`. Every
/// other failure (a store, a bug, key generation) is a plain `anyhow::Error`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// No factory for the network or asset a request named.
    Unsupported,
    /// Imported secret material is not a valid key.
    InvalidSecret,
    /// Address text or network is wrong.
    InvalidAddress,
    /// A watch's start is older than the chain lets an index start.
    InvalidStart,
    /// A node or store is unavailable; retrying may succeed.
    Unavailable,
}

impl ErrorKind {
    /// Whether retrying the same request may succeed.
    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(self, Self::Unavailable)
    }
}

/// A handled wallet failure: its kind and a message without key material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// Stable classification.
    pub kind: ErrorKind,
    /// Human-readable detail; never contains key material.
    pub message: String,
}

impl Error {
    /// Builds an error of `kind`.
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl error::Error for Error {}

/// Where keyed wallets are kept.
pub trait Store: Send + Sync {
    /// Keeps `wallet`'s address and key.
    ///
    /// # Errors
    ///
    /// When the store cannot write.
    fn insert<'a>(&'a self, wallet: &'a dyn Wallet) -> Future<'a, Result<()>>;

    /// Every kept wallet, restored through `factories`.
    ///
    /// # Errors
    ///
    /// When the store cannot read or a key does not restore.
    fn find_all<'a>(&'a self, factories: &'a Factories)
    -> Future<'a, Result<Vec<Arc<dyn Wallet>>>>;
}
