//! [`Factory`] (`base::WalletFactory`) and its wallets (`base::Wallet`).

use std::sync::Arc;

use base::{
    Address, Addressed, Asset, Future, Network, Networked, TransactionError, TransactionErrorKind,
    WalletError, WalletErrorKind,
};
use crypto::{Curve, SecretBytes};

use crate::builder::Builder;
use crate::{FeeLimits, KeyPair, Tokens, rpc::Client};

/// Makes the wallets of one Ethereum network. Their builders send ether and
/// the allowlisted tokens under the fee limits, with nonces and fees from
/// the node.
#[derive(Clone)]
pub struct Factory {
    pub(crate) rpc: Client,
    pub(crate) limits: FeeLimits,
    pub(crate) tokens: Tokens,
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

    /// Any-case `0x` hex; mixed case must carry a valid EIP-55 checksum.
    /// Returns canonical lowercase text.
    fn parse_address(&self, text: &str) -> anyhow::Result<Address> {
        let address = crate::Address::parse(text, self.rpc.network()).map_err(|error| {
            WalletError::new(WalletErrorKind::InvalidAddress, format!("{error:#}"))
        })?;
        Ok(address.into())
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
    /// Ether, or one allowlisted token.
    fn transaction(&self, asset: &Asset) -> Result<Box<dyn base::Builder + '_>, TransactionError> {
        let builder = |contract| -> Box<dyn base::Builder + '_> {
            Box::new(Builder {
                key: &self.key,
                rpc: &self.factory.rpc,
                limits: self.factory.limits,
                asset: asset.clone(),
                contract,
                outputs: base::Outputs::default(),
            })
        };
        if *asset == Asset::native(self.network().clone()) {
            return Ok(builder(None));
        }
        let token = Some(asset)
            .filter(|asset| asset.network == *self.network())
            .and_then(|asset| self.factory.tokens.get(asset));
        let Some(token) = token else {
            return Err(TransactionError::new(
                TransactionErrorKind::Unsupported,
                "this Ethereum wallet sends ether and the configured tokens only",
            ));
        };
        Ok(builder(Some(token)))
    }

    fn private_key(&self) -> (Curve, SecretBytes) {
        (Curve::Secp256k1, self.key.secret())
    }
}
