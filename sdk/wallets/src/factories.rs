use std::{collections::BTreeMap, sync::Arc};

use base::{Address, Broadcaster, Network};

use crate::{Error, ErrorKind, Factory, FutureResult, SecretBytes, Wallet};

use anyhow::Result;

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

/// The wallet factory and broadcaster of every configured network: they make
/// wallets and parse addresses. Live wallets are loaded from a [`crate::WalletsStore`].
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
    ) -> FutureResult<'a, Arc<dyn Wallet>> {
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
    ) -> FutureResult<'a, (Arc<dyn Wallet>, SecretBytes)> {
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
