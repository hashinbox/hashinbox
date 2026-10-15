//! [`Factory`] (`base::WalletFactory`) and its wallets (`base::Wallet`).

use std::sync::Arc;

use base::{
    Address, Addressed, Asset, Future, Network, Networked, Signer, TransactionError,
    TransactionErrorKind, WalletError, WalletErrorKind,
};
use crypto::{Curve, SecretBytes};

use crate::builder::Builder;
use crate::{KeyPair, Tokens, rpc::Client};

/// Makes the Ed25519 wallets of one Solana cluster. Their builders send SOL
/// and the allowlisted tokens, on a blockhash and fees from the node.
#[derive(Clone)]
pub struct Factory {
    pub(crate) rpc: Client,
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

    /// Canonical Base58 of 32 bytes, Solana's only textual form. An
    /// off-curve (program-derived) address parses: it is a valid account.
    fn parse_address(&self, text: &str) -> anyhow::Result<Address> {
        let address = crate::Address::parse(text, self.rpc.network()).map_err(|error| {
            WalletError::new(WalletErrorKind::InvalidAddress, format!("{error:#}"))
        })?;
        Ok(address.into())
    }

    /// SOL, or one allowlisted token.
    fn transaction(&self, asset: &Asset) -> Result<Box<dyn base::Builder>, TransactionError> {
        let network: Network = self.rpc.network().into();
        let builder = |token| -> Box<dyn base::Builder> {
            Box::new(Builder {
                rpc: self.rpc.clone(),
                asset: asset.clone(),
                token,
                default: None,
                transfers: Vec::new(),
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
                "this Solana wallet sends SOL and the configured tokens only",
            ));
        };
        Ok(builder(Some(token)))
    }
}

/// One Solana key and its address.
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
        (Curve::Ed25519, self.key.secret())
    }
}
