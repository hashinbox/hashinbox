//! `wallet_index`: one asset of one address being indexed, with its
//! birthday and the block it is indexed through.

use std::collections::BTreeSet;

use anyhow::bail;
use base::{Address, Addressed, Asset, BlockHeader, Chain, Network, Networked, WalletIndexState};
use diesel::{
    dsl::{not, sql},
    expression::BoxableExpression,
    prelude::*,
    sql_types::{BigInt, Bool, Nullable, Text},
    sqlite::Sqlite,
};
use serde::{Deserialize, Serialize};

use super::{
    Block, Wallet, child, int,
    schema::{block, wallet, wallet_index},
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
    /// Every wallet index whose wallet carries this tag.
    Tag(String),
    /// Every wallet index whose wallet carries no tag at all.
    Untagged {},
    /// In every one of these (AND; empty: all).
    All(Vec<Selector>),
    /// In any of these (OR; empty: none).
    Any(Vec<Selector>),
    /// Not in this one (relative to the network's fully indexed indexes).
    Not(Box<Selector>),
}

impl Selector {
    /// Whether it names wallets on purpose: no empty group (an empty "all
    /// of" would be every wallet, an empty "any of" none), no "not" right
    /// inside a "not", no empty id list, no empty asset, every tag well
    /// formed.
    ///
    /// # Errors
    ///
    /// The first part that does not.
    pub(crate) fn validate(&self) -> Result<(), super::trigger::Invalid> {
        use super::trigger::Invalid;
        match self {
            Self::Ids(ids) if ids.is_empty() => Err(Invalid::new("a wallet list picks no wallet")),
            Self::Ids(_) => Ok(()),
            Self::Asset(asset) if asset.is_empty() => Err(Invalid::new("an asset is named")),
            Self::Asset(_) => Ok(()),
            Self::Tag(tag) if !super::wallet::is_tag(tag) => {
                Err(Invalid::new("a tag is 1 to 64 of [a-z0-9-]"))
            }
            Self::Tag(_) | Self::Untagged {} => Ok(()),
            Self::All(all) if all.is_empty() => Err(Invalid::new(
                "an empty \"all of\" would select every wallet; name them by tag, asset or id",
            )),
            Self::Any(any) if any.is_empty() => {
                Err(Invalid::new("an empty \"any of\" selects no wallet"))
            }
            Self::All(many) | Self::Any(many) => many.iter().try_for_each(Self::validate),
            Self::Not(one) if matches!(**one, Self::Not(_)) => {
                Err(Invalid::new("a \"not\" inside a \"not\" cancels out"))
            }
            Self::Not(one) => one.validate(),
        }
    }
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
            // Through the wallet's tags (`wallet.tags`, a JSON list).
            Self::Tag(name) => Box::new(
                sql::<Bool>(
                    "wallet_index.wallet_id IN \
                     (SELECT w.id FROM wallet w, json_each(w.tags) j WHERE j.value = ",
                )
                .bind::<Text, _>(name.clone())
                .sql(")"),
            ),
            Self::Untagged {} => Box::new(sql::<Bool>(
                "wallet_index.wallet_id NOT IN (SELECT w.id FROM wallet w, json_each(w.tags))",
            )),
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
    /// `"{wallet_id}.{asset}"`, `"{chain}.{network}.{address}.{asset}"`.
    ///
    /// # Errors
    ///
    /// When a part holds a dot.
    pub(crate) fn id(address: &Address, asset: &Asset) -> anyhow::Result<String> {
        child(&Wallet::id(address)?, &[&asset.id])
    }

    /// The id of the same address's index of `asset`.
    ///
    /// # Errors
    ///
    /// When a part holds a dot.
    pub(crate) fn sibling_id(&self, asset: &str) -> anyhow::Result<String> {
        child(&Wallet::id(&self.address)?, &[asset])
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

/// The `wallet_index` table's statements. Its chain, network and address
/// are its wallet's: every read by them joins `wallet`.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    /// One `wallet_index` row.
    #[derive(Queryable, Selectable, Insertable)]
    #[diesel(table_name = wallet_index, check_for_backend(Sqlite))]
    pub(crate) struct Row {
        id: String,
        wallet_id: String,
        asset: String,
        requested_from: i64,
        birthday: i64,
        indexed_at: Option<String>,
        state: i32,
        reason: Option<String>,
    }

    impl Row {
        /// The index of `address` (its wallet's), indexed through `block`
        /// (its `indexed_at`).
        ///
        /// # Errors
        ///
        /// When a stored value does not decode.
        pub(crate) fn progress(
            self,
            address: Address,
            block: Option<&Block>,
        ) -> anyhow::Result<base::WalletIndex> {
            Ok(base::WalletIndex {
                asset: Asset {
                    network: address.network.clone(),
                    id: self.asset,
                },
                address,
                requested_from: uint(self.requested_from)?,
                birthday: uint(self.birthday)?,
                indexed_at: block.map(BlockHeader::try_from).transpose()?,
                state: state(self.state)?,
                reason: self.reason,
            })
        }
    }

    /// The address of a row's wallet.
    #[derive(Queryable, Selectable)]
    #[diesel(table_name = wallet, check_for_backend(Sqlite))]
    struct Owner {
        chain: String,
        network: String,
        address: String,
    }

    /// `state` as stored.
    const fn code(state: WalletIndexState) -> i32 {
        match state {
            WalletIndexState::Indexing => 0,
            WalletIndexState::Paused => 1,
            WalletIndexState::NoData => 2,
        }
    }

    /// A stored `state`.
    ///
    /// # Errors
    ///
    /// For a code no writer stores.
    fn state(code: i32) -> anyhow::Result<WalletIndexState> {
        Ok(match code {
            0 => WalletIndexState::Indexing,
            1 => WalletIndexState::Paused,
            2 => WalletIndexState::NoData,
            _ => bail!("a stored index state {code} is unknown"),
        })
    }

    impl From<(Row, Owner)> for WalletIndex {
        fn from((row, owner): (Row, Owner)) -> Self {
            let network = Network::new(Chain::new(owner.chain), owner.network);
            Self {
                id: row.id,
                address: Address::new(network.clone(), owner.address),
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
    fn indexes(rows: Vec<(Row, Owner)>) -> WalletIndexes {
        rows.into_iter().map(WalletIndex::from).collect()
    }

    #[derive(QueryableByName)]
    struct Next {
        #[diesel(sql_type = Nullable<BigInt>)]
        next: Option<i64>,
    }

    /// An index's address and asset.
    #[derive(QueryableByName)]
    struct Pair {
        #[diesel(sql_type = Text)]
        address: String,
        #[diesel(sql_type = Text)]
        asset: String,
    }

    /// The lowest next position of a network's indexing (state 0) indexes,
    /// at or below a bound (NULL: none).
    const NEXT: &str = "
    SELECT MIN(n) AS next FROM (
      SELECT CASE WHEN wi.indexed_at IS NULL THEN wi.birthday ELSE b.position + 1 END AS n
      FROM wallet_index wi JOIN wallet w ON w.id = wi.wallet_id
      LEFT JOIN block b ON b.id = wi.indexed_at
      WHERE w.chain = ?1 AND w.network = ?2 AND wi.state = 0
    ) WHERE ?3 IS NULL OR n <= ?3";

    /// The (address, asset) of every indexing (state 0) index of a network
    /// that a block applies to: birthday at or below it, and indexed through
    /// its parent block, or unset with no live block from the birthday up to
    /// it.
    const APPLYING: &str = "
    SELECT w.address, wi.asset FROM wallet_index wi JOIN wallet w ON w.id = wi.wallet_id
    WHERE w.chain = ?1 AND w.network = ?2 AND wi.state = 0 AND wi.birthday <= ?3
      AND ((wi.indexed_at IS NOT NULL AND wi.indexed_at = ?4)
        OR (wi.indexed_at IS NULL AND NOT EXISTS (
          SELECT 1 FROM block b
          WHERE b.chain = w.chain AND b.network = w.network AND b.archived_at IS NULL
            AND b.position >= wi.birthday AND b.position < ?3)))";

    /// Moves every index indexed through a live block of a network at or
    /// above a position back to a block (or unset when it is below the
    /// birthday). An index's block is of its own network.
    const REWIND: &str = "
    UPDATE wallet_index SET indexed_at = CASE WHEN ?4 >= birthday THEN ?5 ELSE NULL END
    WHERE indexed_at IN (
      SELECT id FROM block
      WHERE chain = ?1 AND network = ?2 AND archived_at IS NULL AND position >= ?3)";

    /// Keeps `index` of its kept wallet; an index already kept keeps its
    /// birthday and progress.
    ///
    /// # Errors
    ///
    /// When its birthday is out of range, its wallet is not kept, or the
    /// write fails.
    pub(crate) fn insert(
        connection: &mut SqliteConnection,
        index: &base::WalletIndex,
    ) -> anyhow::Result<()> {
        let row = Row {
            id: WalletIndex::id(&index.address, &index.asset)?,
            wallet_id: Wallet::id(&index.address)?,
            asset: index.asset.id.clone(),
            requested_from: int(index.requested_from)?,
            birthday: int(index.birthday)?,
            indexed_at: None,
            state: code(index.state),
            reason: index.reason.clone(),
        };
        diesel::insert_or_ignore_into(wallet_index::table)
            .values(&row)
            .execute(connection)?;
        Ok(())
    }

    /// How many distinct wallets of every network are indexed.
    pub(crate) fn count_addresses(connection: &mut SqliteConnection) -> QueryResult<i64> {
        wallet_index::table
            .select(sql::<BigInt>("COUNT(DISTINCT wallet_id)"))
            .get_result(connection)
    }

    /// Every index of every network.
    #[cfg(test)]
    pub(crate) fn find_every(connection: &mut SqliteConnection) -> QueryResult<WalletIndexes> {
        wallet_index::table
            .inner_join(wallet::table)
            .select((Row::as_select(), Owner::as_select()))
            .order((
                wallet::chain,
                wallet::network,
                wallet::address,
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
        let rows: Vec<(Row, String, Option<Block>)> = wallet_index::table
            .inner_join(wallet::table)
            .left_join(block::table)
            .filter(wallet::chain.eq(network.chain.as_str()))
            .filter(wallet::network.eq(&network.name))
            .select((
                Row::as_select(),
                wallet::address,
                Option::<Block>::as_select(),
            ))
            .order((wallet::address, wallet_index::asset))
            .load(connection)?;
        rows.into_iter()
            .map(|(row, address, block)| {
                row.progress(Address::new(network.clone(), address), block.as_ref())
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
        let rows: Vec<Pair> = diesel::sql_query(APPLYING)
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

    /// Moves the birthday of `index` to `birthday` while it has not started.
    ///
    /// # Errors
    ///
    /// When a value is out of range or the write fails.
    pub(crate) fn move_birthday(
        connection: &mut SqliteConnection,
        index: &base::WalletIndex,
        birthday: u64,
    ) -> anyhow::Result<()> {
        diesel::update(
            wallet_index::table
                .filter(wallet_index::id.eq(WalletIndex::id(&index.address, &index.asset)?))
                .filter(wallet_index::indexed_at.is_null()),
        )
        .set(wallet_index::birthday.eq(int(birthday)?))
        .execute(connection)?;
        Ok(())
    }

    /// Sets the state and reason of the index of `asset` of `address`, or
    /// with `None`, of every index of `address`; how many there are.
    ///
    /// # Errors
    ///
    /// When a part of the address holds a dot, or the write fails.
    pub(crate) fn set_state(
        connection: &mut SqliteConnection,
        address: &Address,
        asset: Option<&str>,
        state: WalletIndexState,
        reason: Option<&str>,
    ) -> anyhow::Result<usize> {
        let changes = (
            wallet_index::state.eq(code(state)),
            wallet_index::reason.eq(reason),
        );
        let wallet_id = Wallet::id(address)?;
        Ok(match asset {
            Some(asset) => diesel::update(wallet_index::table.find(child(&wallet_id, &[asset])?))
                .set(changes)
                .execute(connection)?,
            None => {
                diesel::update(wallet_index::table.filter(wallet_index::wallet_id.eq(wallet_id)))
                    .set(changes)
                    .execute(connection)?
            }
        })
    }

    /// Moves the indexes `pairs` (address, asset) of `network` to block
    /// `block_id`.
    ///
    /// # Errors
    ///
    /// When a part holds a dot, or the write fails.
    pub(crate) fn advance(
        connection: &mut SqliteConnection,
        network: &Network,
        pairs: &BTreeSet<(String, String)>,
        block_id: &str,
    ) -> anyhow::Result<()> {
        for (address, asset) in pairs {
            let address = Address::new(network.clone(), address);
            let id = child(&Wallet::id(&address)?, &[asset])?;
            diesel::update(wallet_index::table.find(id))
                .set(wallet_index::indexed_at.eq(block_id))
                .execute(connection)?;
        }
        Ok(())
    }

    /// Forgets index `id`; how many went (0 or 1).
    pub(crate) fn delete(connection: &mut SqliteConnection, id: &str) -> QueryResult<usize> {
        diesel::delete(wallet_index::table.find(id)).execute(connection)
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
        let picked = wallet_index::table
            .filter(selector.predicate())
            .select(wallet_index::id)
            .into_boxed();
        wallet_index::table
            .inner_join(wallet::table)
            .filter(wallet::chain.eq(network.chain.as_str()))
            .filter(wallet::network.eq(&network.name))
            .filter(wallet_index::indexed_at.eq_any(live))
            .filter(wallet_index::id.eq_any(picked))
            .select((Row::as_select(), Owner::as_select()))
            .order(wallet_index::id)
            .load(connection)
            .map(indexes)
    }
}
