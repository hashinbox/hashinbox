//! `wallet`: every address the server knows on a network, keyed (its key's
//! curve and bytes kept) or watch-only, and its tags.

use std::collections::BTreeSet;

use anyhow::bail;
use base::{Address, Chain, Network};
use crypto::{Curve, SecretBytes};
use diesel::{prelude::*, sqlite::Sqlite};

use super::{Json, key, schema::wallet};

/// Key bytes, zeroized on drop; `None` for a watch-only wallet.
pub(crate) struct Secret(pub(crate) Option<SecretBytes>);

impl From<Option<Vec<u8>>> for Secret {
    fn from(bytes: Option<Vec<u8>>) -> Self {
        Self(bytes.map(SecretBytes::new))
    }
}

/// One wallet: an address, its key when kept, its tags. No `Debug`: it may
/// hold the key.
#[derive(Queryable, Selectable)]
#[diesel(table_name = wallet, check_for_backend(Sqlite))]
pub(crate) struct Wallet {
    pub(crate) id: String,
    pub(crate) chain: String,
    pub(crate) network: String,
    pub(crate) address: String,
    /// `None` exactly when `secret` is.
    pub(crate) curve: Option<String>,
    #[diesel(deserialize_as = Option<Vec<u8>>)]
    pub(crate) secret: Secret,
    /// Sorted, each once, each [`is_tag`].
    pub(crate) tags: Json<BTreeSet<String>>,
}

/// Whether `tag` is a tag: 1 to 64 of `[a-z0-9-]`.
pub(crate) fn is_tag(tag: &str) -> bool {
    (1..=64).contains(&tag.len())
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl Wallet {
    /// The wallet of `address` with its key, untagged.
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
            curve: Some(curve.to_owned()),
            secret: Secret(Some(secret)),
            ..Self::from_address(address)?
        })
    }

    /// The watch-only wallet of `address`, untagged.
    ///
    /// # Errors
    ///
    /// When a part of the address holds a dot.
    pub(crate) fn from_address(address: &Address) -> anyhow::Result<Self> {
        Ok(Self {
            id: Self::id(address)?,
            chain: address.network.chain.as_str().to_owned(),
            network: address.network.name.clone(),
            address: address.value.clone(),
            curve: None,
            secret: Secret(None),
            tags: Json(BTreeSet::new()),
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

    /// Whether its key is kept: it can send.
    pub(crate) const fn keyed(&self) -> bool {
        self.secret.0.is_some()
    }

    /// Its key's curve; `None` for a watch-only wallet.
    ///
    /// # Errors
    ///
    /// When the stored name is unknown.
    pub(crate) fn curve(&self) -> anyhow::Result<Option<Curve>> {
        Ok(match self.curve.as_deref() {
            None => None,
            Some("secp256k1") => Some(Curve::Secp256k1),
            Some("ed25519") => Some(Curve::Ed25519),
            Some(_) => bail!("a stored wallet curve is unknown"),
        })
    }
}

/// A wallet with its key, as kept: its address, curve and key bytes.
impl TryFrom<&dyn base::Wallet> for Wallet {
    type Error = anyhow::Error;

    fn try_from(wallet: &dyn base::Wallet) -> anyhow::Result<Self> {
        let (curve, secret) = wallet.private_key();
        Self::new(wallet.address(), curve, secret)
    }
}

/// A wallet with how far each of its indexes is indexed, in asset order.
pub(crate) struct Indexed {
    pub(crate) wallet: Wallet,
    pub(crate) indexes: Vec<base::WalletIndex>,
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
    use anyhow::ensure;
    use diesel::{
        dsl::sql,
        prelude::*,
        sql_types::{BigInt, Binary, Nullable, Text},
    };

    use super::{Indexed, Json, Wallet, Wallets, is_tag};
    use crate::models::{
        Block,
        schema::{block, wallet, wallet_index},
        wallet_index::{Selector, db::Row},
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

    /// A new row, or the key of a kept watch-only row; a kept key stays.
    const INSERT: &str = "
    INSERT INTO wallet (id, chain, network, address, curve, secret) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
    ON CONFLICT (id) DO UPDATE SET curve = excluded.curve, secret = excluded.secret
    WHERE wallet.secret IS NULL AND excluded.secret IS NOT NULL";

    /// Keeps `wallet` without its tags: a new row, or, for a kept
    /// watch-only row, its key (its tags and indexes stay); a kept key stays.
    /// How many rows changed (0 or 1).
    pub(crate) fn insert(connection: &mut SqliteConnection, wallet: &Wallet) -> QueryResult<usize> {
        diesel::sql_query(INSERT)
            .bind::<Text, _>(&wallet.id)
            .bind::<Text, _>(&wallet.chain)
            .bind::<Text, _>(&wallet.network)
            .bind::<Text, _>(&wallet.address)
            .bind::<Nullable<Text>, _>(wallet.curve.as_deref())
            .bind::<Nullable<Binary>, _>(wallet.secret.0.as_ref().map(|key| key.as_bytes()))
            .execute(connection)
    }

    /// The wallet `id`, if kept.
    pub(crate) fn find_by_id(
        connection: &mut SqliteConnection,
        id: &str,
    ) -> QueryResult<Option<Wallet>> {
        wallet::table
            .find(id)
            .select(Wallet::as_select())
            .first(connection)
            .optional()
    }

    /// The wallet `id` (`None`: every wallet, by id), each with its indexes
    /// and the block each is indexed through: one query.
    ///
    /// # Errors
    ///
    /// When the read fails or a stored value does not decode.
    pub(crate) fn find_with_indexes(
        connection: &mut SqliteConnection,
        id: Option<&str>,
    ) -> anyhow::Result<Vec<Indexed>> {
        let mut query = wallet::table
            .left_join(wallet_index::table.left_join(block::table))
            .select((
                Wallet::as_select(),
                Option::<Row>::as_select(),
                Option::<Block>::as_select(),
            ))
            .order((wallet::id, wallet_index::asset))
            .into_boxed();
        if let Some(id) = id {
            query = query.filter(wallet::id.eq(id.to_owned()));
        }
        let mut wallets: Vec<Indexed> = Vec::new();
        for (row, index, block) in query.load::<(Wallet, Option<Row>, Option<Block>)>(connection)? {
            if wallets.last().is_none_or(|last| last.wallet.id != row.id) {
                wallets.push(Indexed {
                    wallet: row,
                    indexes: Vec::new(),
                });
            }
            if let (Some(last), Some(index)) = (wallets.last_mut(), index) {
                let address = base::Address::from(&last.wallet);
                last.indexes.push(index.progress(address, block.as_ref())?);
            }
        }
        Ok(wallets)
    }

    /// Every kept wallet, keyed or not.
    pub(crate) fn find_all(connection: &mut SqliteConnection) -> QueryResult<Wallets> {
        let wallets = wallet::table
            .order(wallet::id)
            .select(Wallet::as_select())
            .load(connection)?;
        Ok(wallets.into())
    }

    /// Every wallet whose key is kept.
    pub(crate) fn find_keyed(connection: &mut SqliteConnection) -> QueryResult<Wallets> {
        let wallets = wallet::table
            .filter(wallet::secret.is_not_null())
            .order(wallet::id)
            .select(Wallet::as_select())
            .load(connection)?;
        Ok(wallets.into())
    }

    /// How many wallets of every network keep their key.
    pub(crate) fn count_keyed(connection: &mut SqliteConnection) -> QueryResult<i64> {
        wallet::table
            .filter(wallet::secret.is_not_null())
            .count()
            .get_result(connection)
    }

    /// A tag in use.
    #[derive(QueryableByName)]
    struct InUse {
        #[diesel(sql_type = Text)]
        tag: String,
    }

    /// Every tag in use on any wallet, sorted, each once. Reads only the
    /// tags (their index).
    pub(crate) fn find_tags(connection: &mut SqliteConnection) -> QueryResult<Vec<String>> {
        let rows: Vec<InUse> = diesel::sql_query(
            "SELECT DISTINCT j.value AS tag FROM wallet w, json_each(w.tags) j ORDER BY tag",
        )
        .load(connection)?;
        Ok(rows.into_iter().map(|row| row.tag).collect())
    }

    /// How many tags every wallet carries, summed.
    pub(crate) fn count_tags(connection: &mut SqliteConnection) -> QueryResult<i64> {
        wallet::table
            .select(sql::<BigInt>("COALESCE(SUM(json_array_length(tags)), 0)"))
            .get_result(connection)
    }

    /// Adds (`add`) or removes `tag` on wallet `id`; `None` when no such
    /// wallet, else whether its tags changed.
    ///
    /// # Errors
    ///
    /// When `tag` is not [`is_tag`], or the read or write fails.
    pub(crate) fn set_tag(
        connection: &mut SqliteConnection,
        id: &str,
        tag: &str,
        add: bool,
    ) -> anyhow::Result<Option<bool>> {
        ensure!(is_tag(tag), "a tag is 1 to 64 of [a-z0-9-]: {tag:?}");
        let Some(Json(mut tags)) = wallet::table
            .find(id)
            .select(wallet::tags)
            .first::<Json<std::collections::BTreeSet<String>>>(connection)
            .optional()?
        else {
            return Ok(None);
        };
        let changed = if add {
            tags.insert(tag.to_owned())
        } else {
            tags.remove(tag)
        };
        if changed {
            diesel::update(wallet::table.find(id))
                .set(wallet::tags.eq(Json(tags)))
                .execute(connection)?;
        }
        Ok(Some(changed))
    }

    /// The wallets with a wallet index `selector` picks.
    pub(crate) fn find_indexed(
        connection: &mut SqliteConnection,
        selector: IndexedSelector<'_>,
    ) -> QueryResult<Wallets> {
        let mut indexes = wallet_index::table.into_boxed();
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
        let mut wallets = wallet::table
            .filter(wallet::id.eq_any(indexes.select(wallet_index::wallet_id)))
            .into_boxed();
        if let Some(network) = selector.network {
            wallets = wallets
                .filter(wallet::chain.eq(network.chain.as_str().to_owned()))
                .filter(wallet::network.eq(network.name.clone()));
        }
        let wallets = wallets
            .order(wallet::id)
            .select(Wallet::as_select())
            .load(connection)?;
        Ok(wallets.into())
    }

    /// Forgets the wallet `id` and its indexes; how many wallets went (0 or 1).
    pub(crate) fn delete(connection: &mut SqliteConnection, id: &str) -> QueryResult<usize> {
        diesel::delete(wallet::table.find(id)).execute(connection)
    }
}
