//! `wallet_index`: one asset of one address being indexed, with its
//! birthday and the block it is indexed through.

use std::collections::BTreeSet;

use base::{Address, Asset, Network, WalletIndex};
use diesel::{
    prelude::*,
    sql_types::{BigInt, Nullable, Text},
    sqlite::Sqlite,
};

use super::{
    Block, int, key,
    schema::{block, wallet_index},
    uint,
};

/// One `wallet_index` row.
#[derive(Clone, Debug, Queryable, Selectable, Insertable, QueryableByName)]
#[diesel(table_name = wallet_index, check_for_backend(Sqlite))]
pub(crate) struct WalletIndexRow {
    pub(crate) id: String,
    pub(crate) chain: String,
    pub(crate) network: String,
    pub(crate) address: String,
    pub(crate) asset: String,
    pub(crate) birthday: i64,
    pub(crate) indexed_at: Option<String>,
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

impl WalletIndexRow {
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

    /// Keeps `index`; an index already kept keeps its birthday and progress.
    ///
    /// # Errors
    ///
    /// When its birthday is out of range or the write fails.
    pub(crate) fn insert(
        connection: &mut SqliteConnection,
        index: &WalletIndex,
    ) -> anyhow::Result<()> {
        let network = &index.address.network;
        let row = Self {
            id: Self::id(&index.address, &index.asset)?,
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
    pub(crate) fn find_every(connection: &mut SqliteConnection) -> QueryResult<Vec<Self>> {
        wallet_index::table
            .select(Self::as_select())
            .order((
                wallet_index::chain,
                wallet_index::network,
                wallet_index::address,
                wallet_index::asset,
            ))
            .load(connection)
    }

    /// Every index of `network`, with the block it is indexed through.
    ///
    /// # Errors
    ///
    /// When the read fails or a stored value does not decode.
    pub(crate) fn find_all(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> anyhow::Result<Vec<WalletIndex>> {
        let rows: Vec<(Self, Option<Block>)> = of!(network)
            .left_join(block::table)
            .select((Self::as_select(), Option::<Block>::as_select()))
            .order((wallet_index::address, wallet_index::asset))
            .load(connection)?;
        rows.into_iter()
            .map(|(row, block)| {
                Ok(WalletIndex {
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
        let rows: Vec<Self> = diesel::sql_query(APPLYING)
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

    /// Forgets the index of `asset` of `address`.
    pub(crate) fn delete(
        connection: &mut SqliteConnection,
        address: &Address,
        asset: &Asset,
    ) -> anyhow::Result<usize> {
        Ok(
            diesel::delete(wallet_index::table.find(Self::id(address, asset)?))
                .execute(connection)?,
        )
    }
}
