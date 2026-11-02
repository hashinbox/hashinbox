//! `wallet`: keyed wallets, their address and their key's curve and bytes.

use anyhow::bail;
use base::{Address, Chain, Network};
use crypto::{Curve, SecretBytes};
use diesel::{prelude::*, sqlite::Sqlite};

use super::{key, schema::wallet};

/// Key bytes, zeroized on drop.
pub(crate) struct Secret(pub(crate) SecretBytes);

impl From<Vec<u8>> for Secret {
    fn from(bytes: Vec<u8>) -> Self {
        Self(SecretBytes::new(bytes))
    }
}

/// One keyed wallet. No `Debug`: it holds the key.
#[derive(Queryable, Selectable)]
#[diesel(table_name = wallet, check_for_backend(Sqlite))]
pub(crate) struct Wallet {
    pub(crate) id: String,
    pub(crate) chain: String,
    pub(crate) network: String,
    pub(crate) address: String,
    pub(crate) curve: String,
    #[diesel(deserialize_as = Vec<u8>)]
    pub(crate) secret: Secret,
}

impl Wallet {
    /// The wallet of `address` with its key.
    ///
    /// # Errors
    ///
    /// When a part of the address holds a dot.
    pub(crate) fn new(
        address: &Address,
        curve: Curve,
        secret: SecretBytes,
    ) -> anyhow::Result<Self> {
        let curve = match curve {
            Curve::Secp256k1 => "secp256k1",
            Curve::Ed25519 => "ed25519",
        };
        Ok(Self {
            id: Self::id(address)?,
            chain: address.network.chain.as_str().to_owned(),
            network: address.network.name.clone(),
            address: address.value.clone(),
            curve: curve.to_owned(),
            secret: Secret(secret),
        })
    }

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

collection! {
    /// Wallets, ordered by id.
    Wallets(Wallet)
}

/// A wallet row's address.
impl From<&Wallet> for Address {
    fn from(row: &Wallet) -> Self {
        Self::new(
            Network::new(Chain::new(&row.chain), &row.network),
            &row.address,
        )
    }
}

impl Wallets {
    /// Their addresses, in order.
    pub(crate) fn addresses(&self) -> Vec<Address> {
        self.iter().map(Address::from).collect()
    }
}

/// The `wallet` table's statements.
#[allow(
    dead_code,
    reason = "the table's whole API; not every statement has a caller yet"
)]
pub(crate) mod db {
    use diesel::prelude::*;

    use super::{Wallet, Wallets};
    use crate::models::{
        schema::{block, wallet, wallet_index},
        wallet_index::Selector,
    };

    /// What [`find_indexed`] picks; each part set narrows it.
    #[derive(Clone, Copy, Debug, Default)]
    pub(crate) struct IndexedSelector<'a> {
        /// Only this network's wallets.
        pub(crate) network: Option<&'a base::Network>,
        /// Only through wallet indexes indexed through a live block at or
        /// above this position (fully indexed).
        pub(crate) confirmed: Option<i64>,
        /// Only through wallet indexes this selector picks.
        pub(crate) selector: Option<&'a Selector>,
    }

    #[derive(Insertable)]
    #[diesel(table_name = wallet)]
    struct New<'a> {
        id: &'a str,
        chain: &'a str,
        network: &'a str,
        address: &'a str,
        curve: &'a str,
        secret: &'a [u8],
    }

    /// Keeps `wallet`; a kept id is kept once.
    pub(crate) fn insert(connection: &mut SqliteConnection, wallet: &Wallet) -> QueryResult<usize> {
        diesel::insert_or_ignore_into(wallet::table)
            .values(&New {
                id: &wallet.id,
                chain: &wallet.chain,
                network: &wallet.network,
                address: &wallet.address,
                curve: &wallet.curve,
                secret: wallet.secret.0.as_bytes(),
            })
            .execute(connection)
    }

    /// The wallet `id`.
    pub(crate) fn get_by_id(connection: &mut SqliteConnection, id: &str) -> QueryResult<Wallet> {
        wallet::table
            .find(id)
            .select(Wallet::as_select())
            .first(connection)
    }

    /// Every kept wallet.
    pub(crate) fn find_all(connection: &mut SqliteConnection) -> QueryResult<Wallets> {
        let wallets = wallet::table
            .order(wallet::id)
            .select(Wallet::as_select())
            .load(connection)?;
        Ok(wallets.into())
    }

    /// How many wallets (keyed) every network keeps.
    pub(crate) fn count(connection: &mut SqliteConnection) -> QueryResult<i64> {
        wallet::table.count().get_result(connection)
    }

    /// The wallets with a wallet index `selector` picks.
    pub(crate) fn find_indexed(
        connection: &mut SqliteConnection,
        selector: IndexedSelector<'_>,
    ) -> QueryResult<Wallets> {
        let mut indexes = wallet_index::table.into_boxed();
        if let Some(network) = selector.network {
            indexes = indexes
                .filter(wallet_index::chain.eq(network.chain.as_str().to_owned()))
                .filter(wallet_index::network.eq(network.name.clone()));
        }
        if let Some(confirmed) = selector.confirmed {
            let mut live = block::table
                .filter(block::archived_at.is_null())
                .filter(block::position.ge(confirmed))
                .select(block::id.nullable())
                .into_boxed();
            if let Some(network) = selector.network {
                live = live
                    .filter(block::chain.eq(network.chain.as_str().to_owned()))
                    .filter(block::network.eq(network.name.clone()));
            }
            indexes = indexes.filter(wallet_index::indexed_at.eq_any(live));
        }
        if let Some(selector) = selector.selector {
            indexes = indexes.filter(selector.predicate());
        }
        // A wallet's id is its address with its network.
        let owners = indexes.select(
            wallet_index::chain
                .concat(".")
                .concat(wallet_index::network)
                .concat(".")
                .concat(wallet_index::address),
        );
        let wallets = wallet::table
            .filter(wallet::id.eq_any(owners))
            .order(wallet::id)
            .select(Wallet::as_select())
            .load(connection)?;
        Ok(wallets.into())
    }

    /// Forgets the wallet `id`; how many went (0 or 1).
    pub(crate) fn delete(connection: &mut SqliteConnection, id: &str) -> QueryResult<usize> {
        diesel::delete(wallet::table.find(id)).execute(connection)
    }
}
