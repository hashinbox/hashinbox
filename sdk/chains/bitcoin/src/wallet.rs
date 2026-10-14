//! [`Factory`] (`base::WalletFactory`) and its wallets (`base::Wallet`).

use std::sync::Arc;

use base::{
    Address, Addressed, Asset, Future, InputStore, Network, Networked, TransactionError,
    TransactionErrorKind, WalletError, WalletErrorKind,
};
use crypto::{Curve, SecretBytes};

use crate::AddressType;
use crate::builder::Builder;
use crate::{FeeRate, KeyPair, Network as Native, rpc::Client};

/// Makes the wallets of one Bitcoin network and address type. Their builders
/// spend the store's inputs and price fees from the node.
#[derive(Clone)]
pub struct Factory {
    pub(crate) rpc: Client,
    pub(crate) inputs: Arc<dyn InputStore>,
    pub(crate) address_type: AddressType,
    pub(crate) fee_target_blocks: u16,
    pub(crate) fee_floor: FeeRate,
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
    /// BTC only.
    fn transaction(&self, asset: &Asset) -> Result<Box<dyn base::Builder + '_>, TransactionError> {
        if *asset != Asset::native(self.network().clone()) {
            return Err(TransactionError::new(
                TransactionErrorKind::Unsupported,
                "a Bitcoin wallet sends BTC only",
            ));
        }
        Ok(Box::new(Builder {
            key: &self.key,
            rpc: &self.factory.rpc,
            inputs: &*self.factory.inputs,
            fee_target_blocks: self.factory.fee_target_blocks,
            fee_floor: self.factory.fee_floor,
            outputs: base::Outputs::default(),
        }))
    }

    fn private_key(&self) -> (Curve, SecretBytes) {
        (Curve::Secp256k1, self.key.secret())
    }
}
