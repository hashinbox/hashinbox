//! `wallet_index`: one asset of one address being indexed, with its
//! birthday and the block it is indexed through.

use std::collections::BTreeSet;

use base::{Address, Addressed, Asset, Chain, Network, Networked};
use diesel::{
    dsl::not,
    expression::BoxableExpression,
    prelude::*,
    sql_types::{BigInt, Bool, Nullable, Text},
    sqlite::Sqlite,
};
use serde::{Deserialize, Serialize};

use super::{
    Block, int, key,
    schema::{block, tag, wallet_index},
    uint,
};

/// A set of wallet indexes; resolved in SQL through [`Selector::predicate`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Selector {
    /// These wallet indexes, by id (`chain.network.address.asset`).
    Ids(Vec<String>),
    /// Every wallet index of this asset id.
    Asset(String),
    /// Every wallet index whose address carries this tag.
    Tag(String),
    /// In every one of these (AND; empty: all).
    All(Vec<Selector>),
    /// In any of these (OR; empty: none).
    Any(Vec<Selector>),
    /// Not in this one (relative to the network's fully indexed indexes).
    Not(Box<Selector>),
}

/// A boolean condition on one `wallet_index` row.
pub(crate) type Predicate = Box<dyn BoxableExpression<wallet_index::table, Sqlite, SqlType = Bool>>;

impl Selector {
    /// It as one condition on a `wallet_index` row: `All` is AND (empty:
    /// true), `Any` OR (empty: false), `Not` NOT.
    pub(crate) fn predicate(&self) -> Predicate {
        match self {
            Self::Ids(ids) => Box::new(wallet_index::id.eq_any(ids.clone())),
            Self::Asset(asset) => Box::new(wallet_index::asset.eq(asset.clone())),
            // The address with its network: `{chain}.{network}.{address}`.
            Self::Tag(name) => Box::new(
                wallet_index::chain
                    .concat(".")
                    .concat(wallet_index::network)
                    .concat(".")
                    .concat(wallet_index::address)
                    .eq_any(
                        tag::table.filter(tag::name.eq(name.clone())).select(
                            tag::chain
                                .concat(".")
                                .concat(tag::network)
                                .concat(".")
                                .concat(tag::address),
                        ),
                    ),
            ),
            Self::All(all) => all
                .iter()
                .map(Self::predicate)
                .reduce(|a, b| Box::new(a.and(b)))
                .unwrap_or_else(|| Box::new(true.into_sql::<Bool>())),
            Self::Any(any) => any
                .iter()
                .map(Self::predicate)
                .reduce(|a, b| Box::new(a.or(b)))
                .unwrap_or_else(|| Box::new(false.into_sql::<Bool>())),
            Self::Not(inner) => Box::new(not(inner.predicate())),
        }
    }

    /// The one asset it is restricted to: an `Asset` leaf at its top or
    /// directly under a top `All`.
    pub(crate) fn asset(&self) -> Option<&str> {
        match self {
            Self::Asset(asset) => Some(asset),
            Self::All(all) => all.iter().find_map(|one| match one {
                Self::Asset(asset) => Some(asset.as_str()),
                _ => None,
            }),
            _ => None,
        }
    }
}

/// What a selector owns among others: what it picks that no other picks,
/// and what it picks that another picks too.
#[derive(Debug)]
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "playground: wired by the pipe evaluator later")
)]
pub(crate) struct Owned {
    pub(crate) owned: WalletIndexes,
    pub(crate) contested: WalletIndexes,
}

/// One asset of one address being indexed.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct WalletIndex {
    pub(crate) id: String,
    pub(crate) address: Address,
    pub(crate) asset: Asset,
    pub(crate) birthday: i64,
    /// The block it is indexed through; `None`: none yet.
    pub(crate) indexed_at: Option<String>,
}

impl WalletIndex {
    /// The id of the index of `asset` of `address`:
    /// `"{chain}.{network}.{address}.{asset}"`.
    ///
    /// # Errors
    ///
    /// When a part holds a dot.
    pub(crate) fn id(address: &Address, asset: &Asset) -> anyhow::Result<String> {
        key(&[
            address.network.chain.as_str(),
            &address.network.name,
            &address.value,
            &asset.id,
        ])
    }

    /// The id of the same address's index of `asset`.
    ///
    /// # Errors
    ///
    /// When a part holds a dot.
    pub(crate) fn sibling_id(&self, asset: &str) -> anyhow::Result<String> {
        key(&[
            self.address.network.chain.as_str(),
            &self.address.network.name,
            &self.address.value,
            asset,
        ])
    }
}

impl Networked for WalletIndex {
    fn network(&self) -> &Network {
        &self.address.network
    }
}

impl Addressed for WalletIndex {
    fn address(&self) -> &Address {
        &self.address
    }
}

collection! {
    /// Wallet indexes, ordered by id.
    #[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
    WalletIndexes(WalletIndex)
}

impl WalletIndexes {
    /// These, minus the ones in `other`.
    #[must_use]
    pub(crate) fn without(self, other: &Self) -> Self {
        self.into_iter()
            .filter(|index| !other.iter().any(|one| one.id == index.id))
            .collect()
    }
}

/// The `wallet_index` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    /// One `wallet_index` row.
    #[derive(Queryable, Selectable, Insertable, QueryableByName)]
    #[diesel(table_name = wallet_index, check_for_backend(Sqlite))]
    struct Row {
        id: String,
        chain: String,
        network: String,
        address: String,
        asset: String,
        birthday: i64,
        indexed_at: Option<String>,
    }

    impl From<Row> for WalletIndex {
        fn from(row: Row) -> Self {
            let network = Network::new(Chain::new(row.chain), row.network);
            Self {
                id: row.id,
                address: Address::new(network.clone(), row.address),
                asset: Asset {
                    network,
                    id: row.asset,
                },
                birthday: row.birthday,
                indexed_at: row.indexed_at,
            }
        }
    }

    /// `rows` as wallet indexes.
    fn indexes(rows: Vec<Row>) -> WalletIndexes {
        rows.into_iter().map(WalletIndex::from).collect()
    }

    #[derive(QueryableByName)]
    struct Next {
        #[diesel(sql_type = Nullable<BigInt>)]
        next: Option<i64>,
    }

    /// The lowest next position of a network's indexes, at or below a bound
    /// (NULL: none).
    const NEXT: &str = "
    SELECT MIN(n) AS next FROM (
      SELECT CASE WHEN wi.indexed_at IS NULL THEN wi.birthday ELSE b.position + 1 END AS n
      FROM wallet_index wi LEFT JOIN block b ON b.id = wi.indexed_at
      WHERE wi.chain = ?1 AND wi.network = ?2
    ) WHERE ?3 IS NULL OR n <= ?3";

    /// The (address, asset) of every index of a network that a block applies
    /// to: birthday at or below it, and indexed through its parent block, or
    /// unset with no live block from the birthday up to it.
    const APPLYING: &str = "
    SELECT wi.* FROM wallet_index wi
    WHERE wi.chain = ?1 AND wi.network = ?2 AND wi.birthday <= ?3
      AND ((wi.indexed_at IS NOT NULL AND wi.indexed_at = ?4)
        OR (wi.indexed_at IS NULL AND NOT EXISTS (
          SELECT 1 FROM block b
          WHERE b.chain = wi.chain AND b.network = wi.network AND b.archived_at IS NULL
            AND b.position >= wi.birthday AND b.position < ?3)))";

    /// Moves every index of a network indexed through a live block at or above
    /// a position back to a block (or unset when it is below the birthday).
    const REWIND: &str = "
    UPDATE wallet_index SET indexed_at = CASE WHEN ?4 >= birthday THEN ?5 ELSE NULL END
    WHERE chain = ?1 AND network = ?2 AND indexed_at IN (
      SELECT id FROM block
      WHERE chain = ?1 AND network = ?2 AND archived_at IS NULL AND position >= ?3)";

    /// The rows of `network`.
    macro_rules! of {
        ($network:expr) => {
            wallet_index::table
                .filter(wallet_index::chain.eq($network.chain.as_str()))
                .filter(wallet_index::network.eq($network.name.as_str()))
        };
    }

    /// Keeps `index`; an index already kept keeps its birthday and progress.
    ///
    /// # Errors
    ///
    /// When its birthday is out of range or the write fails.
    pub(crate) fn insert(
        connection: &mut SqliteConnection,
        index: &base::WalletIndex,
    ) -> anyhow::Result<()> {
        let network = &index.address.network;
        let row = Row {
            id: WalletIndex::id(&index.address, &index.asset)?,
            chain: network.chain.as_str().to_owned(),
            network: network.name.clone(),
            address: index.address.value.clone(),
            asset: index.asset.id.clone(),
            birthday: int(index.birthday)?,
            indexed_at: None,
        };
        diesel::insert_or_ignore_into(wallet_index::table)
            .values(&row)
            .execute(connection)?;
        Ok(())
    }

    /// Every index of every network.
    pub(crate) fn find_every(connection: &mut SqliteConnection) -> QueryResult<WalletIndexes> {
        wallet_index::table
            .select(Row::as_select())
            .order((
                wallet_index::chain,
                wallet_index::network,
                wallet_index::address,
                wallet_index::asset,
            ))
            .load(connection)
            .map(indexes)
    }

    /// Every index of `network`, with the block it is indexed through.
    ///
    /// # Errors
    ///
    /// When the read fails or a stored value does not decode.
    pub(crate) fn find_all(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> anyhow::Result<Vec<base::WalletIndex>> {
        let rows: Vec<(Row, Option<Block>)> = of!(network)
            .left_join(block::table)
            .select((Row::as_select(), Option::<Block>::as_select()))
            .order((wallet_index::address, wallet_index::asset))
            .load(connection)?;
        rows.into_iter()
            .map(|(row, block)| {
                Ok(base::WalletIndex {
                    address: Address::new(network.clone(), row.address),
                    asset: Asset {
                        network: network.clone(),
                        id: row.asset,
                    },
                    birthday: uint(row.birthday)?,
                    indexed_at: block.map(|block| block.header()).transpose()?,
                })
            })
            .collect()
    }

    /// The lowest next position of `network`'s indexes at or below
    /// `through` (`None`: of all), in one query.
    pub(crate) fn find_next(
        connection: &mut SqliteConnection,
        network: &Network,
        through: Option<i64>,
    ) -> QueryResult<Option<i64>> {
        let row: Next = diesel::sql_query(NEXT)
            .bind::<Text, _>(network.chain.as_str())
            .bind::<Text, _>(&network.name)
            .bind::<Nullable<BigInt>, _>(through)
            .get_result(connection)?;
        Ok(row.next)
    }

    /// The (address, asset) of every index of `network` the block at
    /// `position`, whose parent is live block `parent` (if any), applies to.
    pub(crate) fn find_applying(
        connection: &mut SqliteConnection,
        network: &Network,
        position: i64,
        parent: Option<&str>,
    ) -> QueryResult<BTreeSet<(String, String)>> {
        let rows: Vec<Row> = diesel::sql_query(APPLYING)
            .bind::<Text, _>(network.chain.as_str())
            .bind::<Text, _>(&network.name)
            .bind::<BigInt, _>(position)
            .bind::<Nullable<Text>, _>(parent)
            .load(connection)?;
        Ok(rows
            .into_iter()
            .map(|row| (row.address, row.asset))
            .collect())
    }

    /// Moves the indexes `pairs` of `network` to block `block_id`.
    pub(crate) fn advance(
        connection: &mut SqliteConnection,
        network: &Network,
        pairs: &BTreeSet<(String, String)>,
        block_id: &str,
    ) -> QueryResult<()> {
        for (address, asset) in pairs {
            diesel::update(
                of!(network)
                    .filter(wallet_index::address.eq(address))
                    .filter(wallet_index::asset.eq(asset)),
            )
            .set(wallet_index::indexed_at.eq(block_id))
            .execute(connection)?;
        }
        Ok(())
    }

    /// Rewinds every index of `network` indexed through a live block at or
    /// above `from` to `below` (the live block under `from`), or unsets it.
    pub(crate) fn rewind(
        connection: &mut SqliteConnection,
        network: &Network,
        from: i64,
        below: Option<&Block>,
    ) -> QueryResult<usize> {
        diesel::sql_query(REWIND)
            .bind::<Text, _>(network.chain.as_str())
            .bind::<Text, _>(&network.name)
            .bind::<BigInt, _>(from)
            .bind::<Nullable<BigInt>, _>(below.map(|block| block.position))
            .bind::<Nullable<Text>, _>(below.map(|block| block.id.as_str()))
            .execute(connection)
    }

    /// The fully indexed indexes of `network` that `selector` picks, by id:
    /// indexed through a live block at or above `confirmed` (the universe, so
    /// `Not` is relative to it).
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "playground: wired by the pipe evaluator later")
    )]
    pub(crate) fn select(
        connection: &mut SqliteConnection,
        network: &Network,
        selector: &Selector,
        confirmed: i64,
    ) -> QueryResult<WalletIndexes> {
        let live = block::table
            .filter(block::chain.eq(network.chain.as_str()))
            .filter(block::network.eq(&network.name))
            .filter(block::archived_at.is_null())
            .filter(block::position.ge(confirmed))
            .select(block::id.nullable());
        of!(network)
            .filter(wallet_index::indexed_at.eq_any(live))
            .filter(selector.predicate())
            .select(Row::as_select())
            .order(wallet_index::id)
            .load(connection)
            .map(indexes)
    }

    /// The indexes `selector` picks (as [`WalletIndex::select`]) that any of
    /// `others` picks too: contested, so no trigger owns them.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "playground: wired by the pipe evaluator later")
    )]
    pub(crate) fn contested(
        connection: &mut SqliteConnection,
        network: &Network,
        selector: &Selector,
        others: &[Selector],
        confirmed: i64,
    ) -> QueryResult<WalletIndexes> {
        let both = Selector::All(vec![selector.clone(), Selector::Any(others.to_vec())]);
        select(connection, network, &both, confirmed)
    }

    /// What `selector` owns on `network` at `confirmed`, against `others`
    /// (the network's other enabled triggers' selectors).
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "playground: wired by the pipe evaluator later")
    )]
    pub(crate) fn select_owned(
        connection: &mut SqliteConnection,
        network: &Network,
        selector: &Selector,
        others: &[Selector],
        confirmed: i64,
    ) -> QueryResult<Owned> {
        let contested = contested(connection, network, selector, others, confirmed)?;
        let owned = select(connection, network, selector, confirmed)?.without(&contested);
        Ok(Owned { owned, contested })
    }

    /// Forgets the index of `asset` of `address`.
    pub(crate) fn delete(
        connection: &mut SqliteConnection,
        address: &Address,
        asset: &Asset,
    ) -> anyhow::Result<usize> {
        Ok(
            diesel::delete(wallet_index::table.find(WalletIndex::id(address, asset)?))
                .execute(connection)?,
        )
    }
}
