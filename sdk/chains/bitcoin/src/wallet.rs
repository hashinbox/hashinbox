//! [`Factory`] (`base::WalletFactory`) and its wallets (`base::Wallet`).

use std::sync::Arc;

use base::{
    Address, Addressed, Asset, Future, InputStore, Network, Networked, Signer, TransactionError,
    TransactionErrorKind, WalletError, WalletErrorKind,
};
use crypto::{Curve, SecretBytes};

use crate::AddressType;
use crate::builder::Builder;
use crate::{KeyPair, Network as Native, fee::Fees, rpc::Client};

/// Makes the wallets of one Bitcoin network and address type. Their builders
/// spend the store's inputs and price fees with the factory's one estimator.
#[derive(Clone)]
pub struct Factory {
    pub(crate) rpc: Client,
    /// How long a signed transaction counts as pending.
    pub(crate) pending_ttl: std::time::Duration,
    pub(crate) inputs: Arc<dyn InputStore>,
    pub(crate) address_type: AddressType,
    pub(crate) fees: Arc<Fees>,
}

impl Factory {
    pub(crate) fn native(&self) -> Native {
        self.rpc.network()
    }

    /// `text` as a canonical address of this network.
    pub(crate) fn address(&self, text: &str) -> Result<crate::Address, WalletError> {
        crate::Address::parse(text, self.native())
            .map_err(|error| WalletError::new(WalletErrorKind::InvalidAddress, error.to_string()))
    }
}

impl base::WalletFactory for Factory {
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
            pending_ttl: self.pending_ttl,
            rpc: self.rpc.clone(),
            inputs: Arc::clone(&self.inputs),
            fees: Arc::clone(&self.fees),
            default: None,
            outputs: base::Outputs::default(),
            sources: Vec::new(),
        }))
    }

    fn fees(&self) -> Arc<dyn base::FeeEstimator> {
        self.fees.clone()
    }
}

/// One Bitcoin key and its address.
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
