//! `wallet`: every address the server knows on a network, keyed (its key's
//! curve and bytes kept) or watch-only, its tags, and (read at a tip) its
//! indexes with their balances, progress and legs.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::bail;
use base::{Address, Chain, Network};
use crypto::{Curve, SecretBytes};
use serde::Serialize;

use super::{Json, key, wallet_index::WalletIndex};

/// Key bytes, zeroized on drop; `None` for a watch-only wallet.
pub(crate) struct Secret(pub(crate) Option<SecretBytes>);

impl From<Option<Vec<u8>>> for Secret {
    fn from(bytes: Option<Vec<u8>>) -> Self {
        Self(bytes.map(SecretBytes::new))
    }
}

/// One wallet, as the API shows it: an address, whether its key is kept,
/// its tags, and its indexes (each with its balances, progress and legs;
/// read by [`db::find_at_tip`] and [`db::list_at_tip`], empty from the other
/// reads). The key (`curve`, `secret`) is never serialized, and the API
/// reads leave it out. No `Debug`: it may hold the key.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct Wallet {
    /// `{chain}.{network}.{address}`: what every wallet route takes.
    pub(crate) id: String,
    pub(crate) chain: String,
    pub(crate) network: String,
    /// Canonical address text.
    pub(crate) address: String,
    /// Whether this server holds its key and can send from it.
    pub(crate) keyed: bool,
    /// Its tags, sorted, each once, each [`is_tag`].
    #[schema(value_type = Vec<String>)]
    pub(crate) tags: Json<BTreeSet<String>>,
    /// Its indexes by asset id (`native` or the token id).
    pub(crate) indexes: BTreeMap<String, WalletIndex>,
    /// `None` exactly when `secret` is.
    #[serde(skip)]
    pub(crate) curve: Option<String>,
    #[serde(skip)]
    pub(crate) secret: Secret,
}

/// A network's indexer tip, as of its last step, and its final
/// (confirmed) position: what a read measures progress and confirmed
/// balances against. Either may be unknown.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Tip {
    pub(crate) tip: Option<u64>,
    pub(crate) confirmed: Option<u64>,
}

/// Each network's [`Tip`], by `{chain}.{network}`. A network missing here
/// reads with no tip: null progress against it, `balance` over every live
/// block, null `balance_confirmed`.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Tips(pub(crate) BTreeMap<String, Tip>);

impl Tips {
    /// From each network's indexer `stats` (as `Payments::stats` gives
    /// them): its tip and its confirmed position.
    pub(crate) fn from_stats(stats: BTreeMap<Network, base::Stats>) -> Self {
        Self(
            stats
                .into_iter()
                .map(|(network, stats)| {
                    let tip = Tip {
                        tip: stats.tip,
                        confirmed: stats.confirmed,
                    };
                    (format!("{}.{}", network.chain, network.name), tip)
                })
                .collect(),
        )
    }
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
            keyed: true,
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
            keyed: false,
            tags: Json(BTreeSet::new()),
            indexes: BTreeMap::new(),
            curve: None,
            secret: Secret(None),
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
    use std::collections::{BTreeMap, BTreeSet};

    use anyhow::ensure;
    use diesel::{
        dsl::sql,
        prelude::*,
        sql_types::{BigInt, Binary, Bool, Nullable, Text},
        sqlite::Sqlite,
    };

    use super::{Json, Secret, Tips, Wallet, Wallets, is_tag};
    use crate::models::{
        schema::{block, wallet, wallet_index},
        wallet_index::{self as index, Selector, WalletIndex, db::IndexRow},
    };

    /// One `wallet` row: a [`Wallet`] with its key, without its indexes.
    #[derive(Queryable, Selectable)]
    #[diesel(table_name = wallet, check_for_backend(Sqlite))]
    struct Row {
        id: String,
        chain: String,
        network: String,
        address: String,
        curve: Option<String>,
        #[diesel(deserialize_as = Option<Vec<u8>>)]
        secret: Secret,
        tags: Json<BTreeSet<String>>,
    }

    impl From<Row> for Wallet {
        fn from(row: Row) -> Self {
            Self {
                id: row.id,
                chain: row.chain,
                network: row.network,
                address: row.address,
                keyed: row.secret.0.is_some(),
                tags: row.tags,
                indexes: BTreeMap::new(),
                curve: row.curve,
                secret: row.secret,
            }
        }
    }

    /// `rows` as wallets, indexes not read.
    fn wallets(rows: Vec<Row>) -> Wallets {
        rows.into_iter().map(Wallet::from).collect()
    }

    /// Wallet `?2` left-joined to its indexes, each measured against its
    /// network's entry in `?1` (JSON [`Tips`]: tip and final block): one row per index (one without index columns for a
    /// wallet without indexes), by asset. Each balance is the total of the
    /// index's last live mined leg (at or below the final block for the
    /// confirmed one). No key is read.
    const ONE: &str = r#"
    WITH t AS (
        SELECT
            key AS network_id,
            json_extract(value, '$.tip') AS tip,
            json_extract(value, '$.confirmed') AS confirmed
        FROM json_each(?1)
    )
    SELECT
        w.id,
        w.secret IS NOT NULL AS keyed,
        w.tags,
        wi.id AS index_id,
        wi.wallet_id,
        wi.asset,
        wi.requested_from,
        wi.birthday,
        wi.indexed_at,
        wi.state,
        wi.reason,
        w.chain,
        w.network,
        w.address,
        b.position AS indexed_through,
        CASE
            WHEN b.position IS NULL OR t.tip IS NULL THEN NULL
            ELSE MAX(t.tip - b.position, 0)
        END AS behind_tip,
        t.confirmed AS finality_at,
        COALESCE((
            SELECT l.total
            FROM leg l
            JOIN block lb ON lb.id = l.block
            WHERE l.wallet_index_id = wi.id
              AND lb.archived_at IS NULL
            ORDER BY l.position DESC, l.seq DESC
            LIMIT 1
        ), '0') AS balance,
        CASE
            WHEN t.confirmed IS NULL THEN NULL
            ELSE COALESCE((
                SELECT l.total
                FROM leg l
                JOIN block lb ON lb.id = l.block
                WHERE l.wallet_index_id = wi.id
                  AND l.position <= t.confirmed
                  AND lb.archived_at IS NULL
                ORDER BY l.position DESC, l.seq DESC
                LIMIT 1
            ), '0')
        END AS balance_confirmed
    FROM wallet w
    LEFT JOIN wallet_index wi ON wi.wallet_id = w.id
    LEFT JOIN block b ON b.id = wi.indexed_at
    LEFT JOIN t ON t.network_id = w.chain || '.' || w.network
    WHERE w.id = ?2
    ORDER BY w.id, wi.asset"#;

    /// As [`ONE`], for at most `?3` wallets with an id after `?2`, by id.
    const PAGE: &str = r#"
    WITH t AS (
        SELECT
            key AS network_id,
            json_extract(value, '$.tip') AS tip,
            json_extract(value, '$.confirmed') AS confirmed
        FROM json_each(?1)
    )
    SELECT
        w.id,
        w.secret IS NOT NULL AS keyed,
        w.tags,
        wi.id AS index_id,
        wi.wallet_id,
        wi.asset,
        wi.requested_from,
        wi.birthday,
        wi.indexed_at,
        wi.state,
        wi.reason,
        w.chain,
        w.network,
        w.address,
        b.position AS indexed_through,
        CASE
            WHEN b.position IS NULL OR t.tip IS NULL THEN NULL
            ELSE MAX(t.tip - b.position, 0)
        END AS behind_tip,
        t.confirmed AS finality_at,
        COALESCE((
            SELECT l.total
            FROM leg l
            JOIN block lb ON lb.id = l.block
            WHERE l.wallet_index_id = wi.id
              AND lb.archived_at IS NULL
            ORDER BY l.position DESC, l.seq DESC
            LIMIT 1
        ), '0') AS balance,
        CASE
            WHEN t.confirmed IS NULL THEN NULL
            ELSE COALESCE((
                SELECT l.total
                FROM leg l
                JOIN block lb ON lb.id = l.block
                WHERE l.wallet_index_id = wi.id
                  AND l.position <= t.confirmed
                  AND lb.archived_at IS NULL
                ORDER BY l.position DESC, l.seq DESC
                LIMIT 1
            ), '0')
        END AS balance_confirmed
    FROM (SELECT * FROM wallet WHERE id > ?2 ORDER BY id LIMIT ?3) w
    LEFT JOIN wallet_index wi ON wi.wallet_id = w.id
    LEFT JOIN block b ON b.id = wi.indexed_at
    LEFT JOIN t ON t.network_id = w.chain || '.' || w.network
    ORDER BY w.id, wi.asset"#;

    /// A row of [`ONE`] / [`PAGE`]: a wallet and one of its indexes.
    #[derive(QueryableByName)]
    struct AtTip {
        #[diesel(sql_type = Text)]
        id: String,
        #[diesel(sql_type = Text)]
        chain: String,
        #[diesel(sql_type = Text)]
        network: String,
        #[diesel(sql_type = Text)]
        address: String,
        #[diesel(sql_type = Bool)]
        keyed: bool,
        #[diesel(sql_type = Text)]
        tags: Json<BTreeSet<String>>,
        #[diesel(embed)]
        index: Option<IndexRow>,
    }

    /// `rows` grouped by wallet, each index with its legs (one more query
    /// over the indexes' ids).
    fn group(connection: &mut SqliteConnection, rows: Vec<AtTip>) -> anyhow::Result<Wallets> {
        let ids: Vec<&str> = rows
            .iter()
            .filter_map(|row| row.index.as_ref())
            .map(IndexRow::id)
            .collect();
        let mut by_index = index::db::find_legs(connection, &ids)?;
        let mut wallets: Vec<Wallet> = Vec::new();
        for row in rows {
            if wallets.last().is_none_or(|last| last.id != row.id) {
                wallets.push(Wallet {
                    id: row.id,
                    chain: row.chain,
                    network: row.network,
                    address: row.address,
                    keyed: row.keyed,
                    tags: row.tags,
                    indexes: BTreeMap::new(),
                    curve: None,
                    secret: Secret(None),
                });
            }
            if let (Some(last), Some(index)) = (wallets.last_mut(), row.index) {
                let mut index = WalletIndex::try_from(index)?;
                index.legs = by_index.remove(&index.id).unwrap_or_default();
                last.indexes.insert(index.asset.id.clone(), index);
            }
        }
        Ok(wallets.into())
    }

    /// Wallet `id`, if kept, with its indexes measured against `tips` (each
    /// network's indexer tip and confirmations) and their legs: one query
    /// for the wallet and its indexes, one for their legs, grouped here.
    /// Never its key.
    ///
    /// # Errors
    ///
    /// When a read fails or a stored value does not decode.
    pub(crate) fn find_at_tip(
        connection: &mut SqliteConnection,
        id: &str,
        tips: &Tips,
    ) -> anyhow::Result<Option<Wallet>> {
        let rows: Vec<AtTip> = diesel::sql_query(ONE)
            .bind::<Text, _>(serde_json::to_string(tips)?)
            .bind::<Text, _>(id)
            .load(connection)?;
        Ok(group(connection, rows)?.0.pop())
    }

    /// At most `limit` wallets with an id after `after` (`""`: from the
    /// first), by id, each as [`find_at_tip`]: one query for the page with
    /// its indexes, one for their legs.
    ///
    /// # Errors
    ///
    /// As [`find_at_tip`].
    pub(crate) fn list_at_tip(
        connection: &mut SqliteConnection,
        tips: &Tips,
        after: &str,
        limit: i64,
    ) -> anyhow::Result<Wallets> {
        let rows: Vec<AtTip> = diesel::sql_query(PAGE)
            .bind::<Text, _>(serde_json::to_string(tips)?)
            .bind::<Text, _>(after)
            .bind::<BigInt, _>(limit)
            .load(connection)?;
        group(connection, rows)
    }

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
        let row: Option<Row> = wallet::table
            .find(id)
            .select(Row::as_select())
            .first(connection)
            .optional()?;
        Ok(row.map(Wallet::from))
    }

    /// Every kept wallet, keyed or not, indexes not read.
    pub(crate) fn find_all(connection: &mut SqliteConnection) -> QueryResult<Wallets> {
        let rows = wallet::table
            .order(wallet::id)
            .select(Row::as_select())
            .load(connection)?;
        Ok(wallets(rows))
    }

    /// Every wallet whose key is kept, indexes not read.
    pub(crate) fn find_keyed(connection: &mut SqliteConnection) -> QueryResult<Wallets> {
        let rows = wallet::table
            .filter(wallet::secret.is_not_null())
            .order(wallet::id)
            .select(Row::as_select())
            .load(connection)?;
        Ok(wallets(rows))
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
        let rows = wallets
            .order(wallet::id)
            .select(Row::as_select())
            .load(connection)?;
        Ok(super::db::wallets(rows))
    }

    /// Forgets the wallet `id` and its indexes; how many wallets went (0 or 1).
    pub(crate) fn delete(connection: &mut SqliteConnection, id: &str) -> QueryResult<usize> {
        diesel::delete(wallet::table.find(id)).execute(connection)
    }
}
