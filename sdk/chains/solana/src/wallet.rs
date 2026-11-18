//! [`WalletFactory`] (`base::WalletFactory`) and its wallets (`base::Wallet`).

use std::sync::Arc;

use base::{
    Address, Addressed, Asset, Future, Network, Networked, Signer, TransactionError, WalletError,
    WalletErrorKind,
};
use crypto::{Curve, Ed25519, PrivateKey as _, SecretBytes};

use crate::builder::Builder;
use crate::{KeyPair, Token, Tokens, fee_estimator::Estimator, rpc::Client};

/// Makes the Ed25519 wallets of one Solana cluster. Their builders send SOL
/// and the allowlisted tokens, at the signature price and rate of the
/// factory's one estimator.
#[derive(Clone)]
pub struct WalletFactory {
    pub(crate) rpc: Client,
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

    /// Base58 of the 32-byte secret, or of the 64-byte keypair (the secret,
    /// then its public key, which must match).
    fn from_pk_text(&self, text: &str) -> Future<'_, anyhow::Result<Arc<dyn base::Wallet>>> {
        let secret = base58_key(text);
        Box::pin(async move { self.from_pk(secret?).await })
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
        let builder = |token| -> Box<dyn base::Builder> {
            Box::new(Builder {
                rpc: self.rpc.clone(),
                fees: Arc::clone(&self.fees),
                asset: asset.clone(),
                token,
                default: None,
                payment: None,
                state: None,
                fee: base::Fee::default(),
            })
        };
        let Some(mint) = self.tokens.contract(asset)? else {
            return Ok(builder(None));
        };
        let decimals = self
            .tokens
            .decimals(&mint)
            .and_then(|decimals| u8::try_from(decimals).ok())
            .ok_or_else(|| TransactionError::failed("Solana token decimals above 255"))?;
        Ok(builder(Some(Token {
            mint,
            decimals,
            ticker: None,
            display_decimals: None,
        })))
    }

    fn fees(&self) -> Arc<dyn base::FeeEstimator> {
        self.fees.clone()
    }
}

/// One Solana key and its address.
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
        (Curve::Ed25519, self.key.secret())
    }
}

/// The 32-byte secret of `text`: Base58 of the secret, or of the keypair
/// (the secret, then its public key, which must match).
fn base58_key(text: &str) -> Result<SecretBytes, WalletError> {
    let invalid = |message: &str| WalletError::new(WalletErrorKind::InvalidSecret, message);
    let form = "a Solana key is Base58 of a 32-byte secret or a 64-byte keypair";
    let bytes = SecretBytes::from_base58(text).map_err(|_| invalid(form))?;
    match bytes.as_bytes().len() {
        32 => Ok(bytes),
        64 => Ed25519::from_keypair(bytes.as_bytes())
            .map(|key| key.to_bytes())
            .map_err(|_| invalid("the keypair's public half is not its secret's")),
        _ => Err(invalid(form)),
    }
}
