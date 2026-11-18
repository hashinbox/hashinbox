//! [`WalletFactory`] (`base::WalletFactory`) and its wallets (`base::Wallet`).

use std::sync::Arc;

use base::{
    Address, Addressed, Asset, Future, Network, Networked, Signer, TransactionError, WalletError,
    WalletErrorKind,
};
use crypto::{Curve, SecretBytes};

use crate::builder::Builder;
use crate::{KeyPair, Tokens, fee_estimator::Estimator, rpc::Client};

/// Makes the wallets of one Ethereum network. Their builders send ether and
/// the allowlisted tokens under the fee limits, with nonces from the node and
/// the sends in flight, and fees from the factory's one estimator.
#[derive(Clone)]
pub struct WalletFactory {
    pub(crate) rpc: Client,
    /// The sends in flight: nonces count them.
    pub(crate) transactions: Arc<dyn base::TransactionIndexStore>,
    pub(crate) tokens: Tokens,
    pub(crate) fees: Arc<Estimator>,
}

impl base::WalletFactory for WalletFactory {
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
        let builder = |contract| -> Box<dyn base::Builder> {
            Box::new(Builder {
                rpc: self.rpc.clone(),
                transactions: Arc::clone(&self.transactions),
                fees: Arc::clone(&self.fees),
                asset: asset.clone(),
                contract,
                default: None,
                payment: None,
                state: None,
                fee: base::Fee::default(),
            })
        };
        Ok(builder(self.tokens.contract(asset)?))
    }

    fn fees(&self) -> Arc<dyn base::FeeEstimator> {
        self.fees.clone()
    }
}

/// One Ethereum key and its address.
pub(crate) struct Wallet {
    pub(crate) key: KeyPair,
    pub(crate) factory: WalletFactory,
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
