//! `wallet`: keyed wallets, their address and their key's curve and bytes.

use anyhow::bail;
use base::{Address, Chain, Network};
use crypto::{Curve, SecretBytes};
use diesel::{prelude::*, sqlite::Sqlite};

use super::{key, schema::wallet};

/// Key bytes read from the file, zeroized on drop.
pub(crate) struct Secret(pub(crate) SecretBytes);

impl From<Vec<u8>> for Secret {
    fn from(bytes: Vec<u8>) -> Self {
        Self(SecretBytes::new(bytes))
    }
}

/// One `wallet` row. No `Debug`: it holds the key.
#[derive(Queryable, Selectable)]
#[diesel(table_name = wallet, check_for_backend(Sqlite))]
pub(crate) struct WalletRow {
    pub(crate) chain: String,
    pub(crate) network: String,
    pub(crate) address: String,
    pub(crate) curve: String,
    #[diesel(deserialize_as = Vec<u8>)]
    pub(crate) secret: Secret,
}

#[derive(Insertable)]
#[diesel(table_name = wallet)]
struct NewWallet<'a> {
    id: &'a str,
    chain: &'a str,
    network: &'a str,
    address: &'a str,
    curve: &'a str,
    secret: &'a [u8],
}

impl WalletRow {
    /// The id of the wallet of `address`: `"{chain}.{network}.{address}"`.
    ///
    /// # Errors
    ///
    /// When a part holds a dot.
    pub(crate) fn id(address: &Address) -> anyhow::Result<String> {
        key(&[
            address.network.chain.as_str(),
            &address.network.name,
            &address.value,
        ])
    }

    /// Keeps `address` with its key; a kept address is kept once.
    ///
    /// # Errors
    ///
    /// When an id part holds a dot or the write fails.
    pub(crate) fn insert(
        connection: &mut SqliteConnection,
        address: &Address,
        curve: Curve,
        secret: &SecretBytes,
    ) -> anyhow::Result<usize> {
        let curve = match curve {
            Curve::Secp256k1 => "secp256k1",
            Curve::Ed25519 => "ed25519",
        };
        let id = Self::id(address)?;
        Ok(diesel::insert_or_ignore_into(wallet::table)
            .values(&NewWallet {
                id: &id,
                chain: address.network.chain.as_str(),
                network: &address.network.name,
                address: &address.value,
                curve,
                secret: secret.as_bytes(),
            })
            .execute(connection)?)
    }

    /// Every kept wallet.
    pub(crate) fn find_all(connection: &mut SqliteConnection) -> QueryResult<Vec<Self>> {
        wallet::table.select(Self::as_select()).load(connection)
    }

    /// Forgets the wallet of `address`.
    pub(crate) fn delete(
        connection: &mut SqliteConnection,
        address: &Address,
    ) -> anyhow::Result<usize> {
        Ok(diesel::delete(wallet::table.find(Self::id(address)?)).execute(connection)?)
    }

    /// Its address.
    pub(crate) fn address(&self) -> Address {
        Address::new(
            Network::new(Chain::new(&self.chain), &self.network),
            &self.address,
        )
    }

    /// Its key's curve.
    ///
    /// # Errors
    ///
    /// When the stored name is unknown.
    pub(crate) fn curve(&self) -> anyhow::Result<Curve> {
        Ok(match self.curve.as_str() {
            "secp256k1" => Curve::Secp256k1,
            "ed25519" => Curve::Ed25519,
            _ => bail!("a stored wallet curve is unknown"),
        })
    }
}
