//! [`WalletFactory`] (`base::WalletFactory`) and its wallets (`base::Wallet`).

use std::sync::Arc;

use base::{
    Address, Addressed, Asset, Future, InputStore, Network, Networked, Signer, TransactionError,
    TransactionErrorKind, WalletError, WalletErrorKind,
};
use crypto::{Curve, SecretBytes};

use crate::AddressType;
use crate::builder::Builder;
use crate::{FeeRate, KeyPair, Network as Native, fee_estimator::Estimator, rpc::Client};

/// Makes the wallets of one Bitcoin network and address type. Their builders
/// spend the store's inputs and price fees at the factory's one cached rate,
/// as its estimator does.
#[derive(Clone)]
pub struct WalletFactory {
    pub(crate) rpc: Client,
    pub(crate) inputs: Arc<dyn InputStore>,
    pub(crate) address_type: AddressType,
    pub(crate) rates: Arc<dyn base::FeeRateProvider>,
    pub(crate) floor: FeeRate,
    pub(crate) fees: Arc<Estimator>,
}

impl WalletFactory {
    pub(crate) fn native(&self) -> Native {
        self.rpc.network()
    }

    /// `text` as a canonical address of this network.
    pub(crate) fn address(&self, text: &str) -> Result<crate::Address, WalletError> {
        crate::Address::parse(text, self.native())
            .map_err(|error| WalletError::new(WalletErrorKind::InvalidAddress, error.to_string()))
    }
}

impl base::WalletFactory for WalletFactory {
    fn generate(&self) -> Future<'_, anyhow::Result<SecretBytes>> {
        Box::pin(async move { Ok(KeyPair::generate(self.native(), self.address_type)?.secret()) })
    }

    fn from_pk(&self, secret: SecretBytes) -> Future<'_, anyhow::Result<Arc<dyn base::Wallet>>> {
        Box::pin(async move {
            let key =
                KeyPair::import(&secret, self.native(), self.address_type).map_err(|error| {
                    WalletError::new(WalletErrorKind::InvalidSecret, error.to_string())
                })?;
            let wallet: Arc<dyn base::Wallet> = Arc::new(Wallet {
                key,
                factory: self.clone(),
            });
            Ok(wallet)
        })
    }

    /// WIF of a compressed key of this network.
    fn from_pk_text(&self, text: &str) -> Future<'_, anyhow::Result<Arc<dyn base::Wallet>>> {
        let secret = wif(text, self.native());
        Box::pin(async move { self.from_pk(secret?).await })
    }

    /// Any-case text of an address of this network; returns canonical text.
    fn parse_address(&self, text: &str) -> anyhow::Result<Address> {
        Ok(self.address(text)?.into())
    }

    /// BTC only.
    fn transaction(&self, asset: &Asset) -> Result<Box<dyn base::Builder>, TransactionError> {
        if *asset != Asset::native(self.native().into()) {
            return Err(TransactionError::new(
                TransactionErrorKind::Unsupported,
                "a Bitcoin wallet sends BTC only",
            ));
        }
        Ok(Box::new(Builder {
            rpc: self.rpc.clone(),
            inputs: Arc::clone(&self.inputs),
            rates: Arc::clone(&self.rates),
            floor: self.floor,
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

/// One Bitcoin key and its address.
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

/// The 32-byte secret of `text`: WIF of a compressed key of `network`.
fn wif(text: &str, network: Native) -> Result<SecretBytes, WalletError> {
    let invalid = |message: &str| WalletError::new(WalletErrorKind::InvalidSecret, message);
    let key =
        bitcoin::PrivateKey::from_wif(text.trim()).map_err(|_| invalid("a Bitcoin key is WIF"))?;
    if !key.compressed {
        return Err(invalid(
            "an uncompressed WIF key derives no SegWit or Taproot address",
        ));
    }
    if key.network != bitcoin::NetworkKind::from(network.native()) {
        return Err(invalid("the WIF key is of another network"));
    }
    Ok(SecretBytes::new(key.inner.secret_bytes()))
}
