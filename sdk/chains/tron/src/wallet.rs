//! [`WalletFactory`] (`base::WalletFactory`) and its wallets (`base::Wallet`).

use std::sync::Arc;

use base::{
    Address, Addressed, Asset, Decimal, Future, Network, Networked, Signer, TransactionError,
    WalletError, WalletErrorKind,
};
use crypto::{Curve, SecretBytes};

use crate::builder::Builder;
use crate::{KeyPair, Tokens, fee::Estimator, rpc::Client};

/// Makes the wallets of one Tron network. Their builders send TRX and the
/// allowlisted tokens, priced by the factory's one estimator.
#[derive(Clone)]
pub struct WalletFactory {
    pub(crate) rpc: Client,
    pub(crate) tokens: Tokens,
    pub(crate) fees: Arc<Estimator>,
}

impl base::WalletFactory for WalletFactory {
    /// Whether one payment of `asset` carries `units` atomic units: TRX at
    /// most `i64::MAX` sun (a `TransferContract`'s `int64`), a token a
    /// `uint256`; neither for an asset this network does not send.
    fn carries(&self, asset: &Asset, units: &Decimal) -> bool {
        match self.tokens.contract(asset) {
            Ok(None) => units
                .to_atomic_u64(0)
                .is_ok_and(|sun| i64::try_from(sun).is_ok()),
            Ok(Some(_)) => units.to_atomic_be_bytes::<32>(0).is_ok(),
            Err(_) => false,
        }
    }

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
                    "a Tron key is 64 hexadecimal digits, 0x optional",
                )
            });
        Box::pin(async move { self.from_pk(secret?).await })
    }

    /// `T…` Base58Check, or `41` and 40 hex digits. Returns the `T…` text.
    fn parse_address(&self, text: &str) -> anyhow::Result<Address> {
        let address = crate::Address::parse(text, self.rpc.network()).map_err(|error| {
            WalletError::new(WalletErrorKind::InvalidAddress, format!("{error:#}"))
        })?;
        Ok(address.into())
    }

    /// TRX, or one allowlisted token.
    fn transaction(&self, asset: &Asset) -> Result<Box<dyn base::Builder>, TransactionError> {
        Ok(Box::new(Builder {
            rpc: self.rpc.clone(),
            fees: Arc::clone(&self.fees),
            asset: asset.clone(),
            contract: self.tokens.contract(asset)?,
            default: None,
            payment: None,
            state: None,
            fee: base::Fee::default(),
        }))
    }

    fn fees(&self) -> Arc<dyn base::FeeEstimator> {
        self.fees.clone()
    }
}

/// One Tron key and its address.
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
