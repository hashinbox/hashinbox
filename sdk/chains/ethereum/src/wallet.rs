//! [`Factory`] (`base::WalletFactory`) and its wallets (`base::Wallet`).

use std::sync::Arc;

use base::{
    Address, Addressed, Asset, Future, Network, Networked, Signer, TransactionError,
    TransactionErrorKind, WalletError, WalletErrorKind,
};
use crypto::{Curve, SecretBytes};

use crate::builder::Builder;
use crate::{KeyPair, Tokens, fee_cache::Cached, rpc::Client};

/// Makes the wallets of one Ethereum network. Their builders send ether and
/// the allowlisted tokens under the fee limits, with nonces from the node and
/// fees from the factory's one estimator.
#[derive(Clone)]
pub struct Factory {
    pub(crate) rpc: Client,
    /// How long a signed transaction counts as pending.
    pub(crate) pending_ttl: std::time::Duration,
    pub(crate) tokens: Tokens,
    pub(crate) fees: Arc<Cached>,
}

impl base::WalletFactory for Factory {
    fn generate(&self) -> Future<'_, anyhow::Result<SecretBytes>> {
        Box::pin(async move { Ok(KeyPair::generate(self.rpc.network())?.secret()) })
    }

    fn from_pk(&self, secret: SecretBytes) -> Future<'_, anyhow::Result<Arc<dyn base::Wallet>>> {
        Box::pin(async move {
            let key = KeyPair::import(&secret, self.rpc.network()).map_err(|error| {
                WalletError::new(WalletErrorKind::InvalidSecret, error.to_string())
            })?;
            let wallet: Arc<dyn base::Wallet> = Arc::new(Wallet {
                key,
                factory: self.clone(),
            });
            Ok(wallet)
        })
    }

    /// 64 hexadecimal digits, `0x` optional.
    fn from_pk_text(&self, text: &str) -> Future<'_, anyhow::Result<Arc<dyn base::Wallet>>> {
        let text = text.trim();
        let digits = text
            .strip_prefix("0x")
            .or_else(|| text.strip_prefix("0X"))
            .unwrap_or(text);
        let secret = SecretBytes::from_hex(digits)
            .ok()
            .filter(|secret| secret.as_bytes().len() == 32)
            .ok_or_else(|| {
                WalletError::new(
                    WalletErrorKind::InvalidSecret,
                    "an Ethereum key is 64 hexadecimal digits, 0x optional",
                )
            });
        Box::pin(async move { self.from_pk(secret?).await })
    }

    /// Any-case `0x` hex; mixed case must carry a valid EIP-55 checksum.
    /// Returns canonical lowercase text.
    fn parse_address(&self, text: &str) -> anyhow::Result<Address> {
        let address = crate::Address::parse(text, self.rpc.network()).map_err(|error| {
            WalletError::new(WalletErrorKind::InvalidAddress, format!("{error:#}"))
        })?;
        Ok(address.into())
    }

    /// Ether, or one allowlisted token.
    fn transaction(&self, asset: &Asset) -> Result<Box<dyn base::Builder>, TransactionError> {
        let network: Network = self.rpc.network().into();
        let builder = |contract| -> Box<dyn base::Builder> {
            Box::new(Builder {
                pending_ttl: self.pending_ttl,
                rpc: self.rpc.clone(),
                fees: Arc::clone(&self.fees),
                asset: asset.clone(),
                contract,
                default: None,
                outputs: base::Outputs::default(),
                sources: Vec::new(),
            })
        };
        if *asset == Asset::native(network.clone()) {
            return Ok(builder(None));
        }
        let token = Some(asset)
            .filter(|asset| asset.network == network)
            .and_then(|asset| self.tokens.get(asset));
        let Some(token) = token else {
            return Err(TransactionError::new(
                TransactionErrorKind::Unsupported,
                "this Ethereum wallet sends ether and the configured tokens only",
            ));
        };
        Ok(builder(Some(token)))
    }

    fn fees(&self) -> Arc<dyn base::FeeEstimator> {
        self.fees.clone()
    }
}

/// One Ethereum key and its address.
pub(crate) struct Wallet {
    pub(crate) key: KeyPair,
    pub(crate) factory: Factory,
}

impl Addressed for Wallet {
    fn address(&self) -> &Address {
        self.key.address()
    }
}

impl Networked for Wallet {
    fn network(&self) -> &Network {
        self.key.network()
    }
}

impl base::Wallet for Wallet {
    fn transaction(&self, asset: &Asset) -> Result<Box<dyn base::Builder>, TransactionError> {
        let mut builder = base::WalletFactory::transaction(&self.factory, asset)?;
        builder.with_default_source(Signer::from(self));
        Ok(builder)
    }

    fn private_key(&self) -> (Curve, SecretBytes) {
        (Curve::Secp256k1, self.key.secret())
    }
}
