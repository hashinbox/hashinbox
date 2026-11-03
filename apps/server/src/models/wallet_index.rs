//! `wallet_index`: one asset of one address being indexed, with its
//! birthday and the block it is indexed through.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, bail, ensure};
use base::{
    Address, Addressed, Asset, BlockHeader, Chain, Decimal, Network, Networked, WalletIndexState,
};
use diesel::{
    dsl::{not, sql},
    expression::BoxableExpression,
    prelude::*,
    sql_types::{BigInt, Bool, Integer, Nullable, Text},
    sqlite::Sqlite,
};
use serde::{Deserialize, Serialize};

use super::{
    Block, Leg, Wallet, child, int,
    schema::{block, wallet, wallet_index},
    trigger::{Target, This},
    uint,
    wallet::{Tip, Tips},
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

/// Whether an index is scanned.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IndexState {
    /// Indexed.
    Indexing,
    /// Skipped until resumed; resumed, it backfills what it missed.
    Paused,
    /// Skipped: the node no longer serves a block it needs (see `reason`);
    /// resume once a node serves it.
    NoData,
}

impl From<WalletIndexState> for IndexState {
    fn from(state: WalletIndexState) -> Self {
        match state {
            WalletIndexState::Indexing => Self::Indexing,
            WalletIndexState::Paused => Self::Paused,
            WalletIndexState::NoData => Self::NoData,
        }
    }
}

impl From<IndexState> for WalletIndexState {
    fn from(state: IndexState) -> Self {
        match state {
            IndexState::Indexing => Self::Indexing,
            IndexState::Paused => Self::Paused,
            IndexState::NoData => Self::NoData,
        }
    }
}

/// Serializes an asset as its id.
fn asset_id<S: serde::Serializer>(asset: &Asset, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&asset.id)
}

/// One asset of one wallet being indexed, as every read answers it (the
/// API shows it as is): its columns, how far it is indexed against its
/// network's tip, its balances, and (from the wallet reads) its legs. All
/// computed in SQL (`db::BY_IDS`); amounts are exact atomic decimal
/// text, never summed. Positions are native (height or slot).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub(crate) struct WalletIndex {
    /// `{wallet_id}.{asset}`.
    pub(crate) id: String,
    /// `{chain}.{network}.{address}`.
    pub(crate) wallet_id: String,
    /// The asset id.
    #[serde(serialize_with = "asset_id")]
    #[schema(value_type = String)]
    pub(crate) asset: Asset,
    pub(crate) state: IndexState,
    /// Why it is `no_data`; null otherwise.
    pub(crate) reason: Option<String>,
    /// The start asked for.
    pub(crate) requested_from: i64,
    /// The start indexed from: the one asked for, or the first block the
    /// node served when it no longer served that one.
    pub(crate) birthday: i64,
    /// The last block indexed; null before the first.
    pub(crate) indexed_through: Option<i64>,
    /// Blocks between `indexed_through` and the network's produced tip;
    /// null before the first block or without a tip.
    pub(crate) behind_tip: Option<i64>,
    /// The network's final (confirmed) block the read was given; null when
    /// unknown.
    pub(crate) finality_at: Option<i64>,
    /// Its last live mined leg's total, any block: `0` without one;
    /// negative when indexed debits exceed indexed credits (history before
    /// the start is unknown).
    pub(crate) balance: String,
    /// The same at or below `finality_at`; null when that is unknown.
    pub(crate) balance_confirmed: Option<String>,
    /// Its legs in (position, seq) order, pending ones (no block, no total)
    /// last; read by the wallet reads only, empty otherwise.
    pub(crate) legs: Vec<Leg>,
    /// Its wallet's address.
    #[serde(skip)]
    pub(crate) address: Address,
    /// The block it is indexed through.
    #[serde(skip)]
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
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    WalletIndexes(WalletIndex)
}

impl WalletIndex {
    /// What it can spend: `balance_confirmed` (zero when no final block is
    /// known) less what its pending sends take, its pending outgoing legs
    /// (spent, burned, fee; no block). What pending sends bring counts once
    /// mined.
    ///
    /// # Errors
    ///
    /// When a stored amount does not decode.
    pub(crate) fn spendable(&self) -> anyhow::Result<Decimal> {
        let confirmed: Decimal = self
            .balance_confirmed
            .as_deref()
            .unwrap_or("0")
            .parse()
            .context("a stored total does not decode")?;
        self.legs
            .iter()
            .filter(|leg| leg.block.is_none() && matches!(leg.kind, 1 | 2 | 4))
            .try_fold(confirmed, |sum, leg| Ok(sum + leg.signed()?))
    }
}

/// Wallet indexes of one network read at one final block, by id: what a
/// trigger selects and decides on (`This`). Their legs are the pending ones
/// only: what [`WalletIndex::spendable`] reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Selection {
    network: Network,
    /// The final block position the balances are at.
    confirmed: i64,
    indexes: BTreeMap<String, WalletIndex>,
}

impl Selection {
    /// None yet, of `network` at final block `confirmed`.
    pub(crate) fn new(network: Network, confirmed: i64) -> Self {
        Self {
            network,
            confirmed,
            indexes: BTreeMap::new(),
        }
    }

    /// The final block position the balances are at.
    pub(crate) fn confirmed_at(&self) -> i64 {
        self.confirmed
    }

    /// None, of the same network at the same block.
    #[must_use]
    pub(crate) fn emptied(&self) -> Self {
        Self::new(self.network.clone(), self.confirmed)
    }

    /// `one` alone, of the same network at the same block.
    ///
    /// # Errors
    ///
    /// When it is of another network.
    pub(crate) fn only(&self, one: &WalletIndex) -> anyhow::Result<Self> {
        let mut alone = self.emptied();
        alone.insert(one.clone())?;
        Ok(alone)
    }

    /// Adds `one`.
    ///
    /// # Errors
    ///
    /// When it is of another network.
    pub(crate) fn insert(&mut self, one: WalletIndex) -> anyhow::Result<()> {
        ensure!(
            *one.network() == self.network,
            "a wallet index is of another network"
        );
        self.indexes.insert(one.id.clone(), one);
        Ok(())
    }

    /// Their ids, in order.
    pub(crate) fn ids(&self) -> Vec<String> {
        self.indexes.keys().cloned().collect()
    }

    /// Their spendable balances ([`WalletIndex::spendable`]), in id order.
    ///
    /// # Errors
    ///
    /// When a stored amount does not decode.
    pub(crate) fn spendable(&self) -> anyhow::Result<Vec<Decimal>> {
        self.indexes.values().map(WalletIndex::spendable).collect()
    }

    /// Each one's (address, asset), what a fee estimate takes.
    pub(crate) fn sources(&self) -> Vec<(Address, Asset)> {
        self.indexes
            .values()
            .map(|one| (one.address.clone(), one.asset.clone()))
            .collect()
    }
}

impl std::ops::Deref for Selection {
    type Target = BTreeMap<String, WalletIndex>;

    fn deref(&self) -> &Self::Target {
        &self.indexes
    }
}

impl Networked for Selection {
    fn network(&self) -> &Network {
        &self.network
    }
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

    /// The indexes `?2` (a JSON list of ids), each measured against its
    /// network's entry in tips `?1` (JSON: tip and final block), by wallet
    /// then asset. Each balance is the total of the index's last live mined
    /// leg (at or below the final block for the confirmed one).
    const BY_IDS: &str = r#"
    WITH t AS (
        SELECT
            key AS network_id,
            json_extract(value, '$.tip') AS tip,
            json_extract(value, '$.confirmed') AS confirmed
        FROM json_each(?1)
    )
    SELECT
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
    FROM wallet_index wi
    JOIN wallet w ON w.id = wi.wallet_id
    LEFT JOIN block b ON b.id = wi.indexed_at
    LEFT JOIN t ON t.network_id = w.chain || '.' || w.network
    WHERE wi.id IN (SELECT value FROM json_each(?2))
    ORDER BY wi.wallet_id, wi.asset"#;

    impl IndexRow {
        /// Its index's id.
        pub(crate) fn id(&self) -> &str {
            &self.index_id
        }
    }

    /// A row of [`BY_IDS`]: a [`WalletIndex`] without its legs.
    #[derive(QueryableByName)]
    pub(crate) struct IndexRow {
        #[diesel(sql_type = Text)]
        index_id: String,
        #[diesel(sql_type = Text)]
        wallet_id: String,
        #[diesel(sql_type = Text)]
        asset: String,
        #[diesel(sql_type = BigInt)]
        requested_from: i64,
        #[diesel(sql_type = BigInt)]
        birthday: i64,
        #[diesel(sql_type = Nullable<Text>)]
        indexed_at: Option<String>,
        #[diesel(sql_type = Integer)]
        state: i32,
        #[diesel(sql_type = Nullable<Text>)]
        reason: Option<String>,
        #[diesel(sql_type = Text)]
        chain: String,
        #[diesel(sql_type = Text)]
        network: String,
        #[diesel(sql_type = Text)]
        address: String,
        #[diesel(sql_type = Nullable<BigInt>)]
        indexed_through: Option<i64>,
        #[diesel(sql_type = Nullable<BigInt>)]
        behind_tip: Option<i64>,
        #[diesel(sql_type = Nullable<BigInt>)]
        finality_at: Option<i64>,
        #[diesel(sql_type = Text)]
        balance: String,
        #[diesel(sql_type = Nullable<Text>)]
        balance_confirmed: Option<String>,
    }

    impl TryFrom<IndexRow> for WalletIndex {
        type Error = anyhow::Error;

        fn try_from(row: IndexRow) -> anyhow::Result<Self> {
            let network = Network::new(Chain::new(row.chain), row.network);
            Ok(Self {
                id: row.index_id,
                wallet_id: row.wallet_id,
                address: Address::new(network.clone(), row.address),
                asset: Asset {
                    network,
                    id: row.asset,
                },
                state: state(row.state)?.into(),
                reason: row.reason,
                requested_from: row.requested_from,
                birthday: row.birthday,
                indexed_through: row.indexed_through,
                behind_tip: row.behind_tip,
                finality_at: row.finality_at,
                balance: row.balance,
                balance_confirmed: row.balance_confirmed,
                legs: Vec::new(),
                indexed_at: row.indexed_at,
            })
        }
    }

    /// `rows` as wallet indexes.
    fn decode(rows: Vec<IndexRow>) -> anyhow::Result<WalletIndexes> {
        rows.into_iter().map(WalletIndex::try_from).collect()
    }

    /// The legs of indexes `ids`, by index id: each index's live mined legs
    /// in (position, seq) order, then its pending ones.
    ///
    /// # Errors
    ///
    /// When the read fails.
    pub(crate) fn find_legs(
        connection: &mut SqliteConnection,
        ids: &[&str],
    ) -> anyhow::Result<BTreeMap<String, Vec<Leg>>> {
        use crate::models::schema::{block, leg};
        // A pending leg has no block, so its joined `archived_at` is NULL too.
        let rows: Vec<Leg> = leg::table
            .left_join(block::table)
            .filter(leg::wallet_index_id.eq_any(ids))
            .filter(block::archived_at.is_null())
            .order((
                leg::wallet_index_id,
                leg::position.is_null(),
                leg::position,
                leg::seq,
                leg::id,
            ))
            .select(Leg::as_select())
            .load(connection)?;
        Ok(by_index(rows))
    }

    /// The pending legs of indexes `ids`, by index id.
    fn find_pending_legs(
        connection: &mut SqliteConnection,
        ids: &[&str],
    ) -> anyhow::Result<BTreeMap<String, Vec<Leg>>> {
        use crate::models::schema::leg;
        let rows: Vec<Leg> = leg::table
            .filter(leg::wallet_index_id.eq_any(ids))
            .filter(leg::block.is_null())
            .order((leg::wallet_index_id, leg::id))
            .select(Leg::as_select())
            .load(connection)?;
        Ok(by_index(rows))
    }

    /// `legs` grouped by their index, in the order read.
    fn by_index(legs: Vec<Leg>) -> BTreeMap<String, Vec<Leg>> {
        let mut grouped: BTreeMap<String, Vec<Leg>> = BTreeMap::new();
        for leg in legs {
            grouped
                .entry(leg.wallet_index_id.clone())
                .or_default()
                .push(leg);
        }
        grouped
    }

    /// Index `id` (`{wallet_id}.{asset}`), if kept, measured against `tips`
    /// as the wallet reads measure it, with its legs: one query for the
    /// index, one for its legs.
    ///
    /// # Errors
    ///
    /// When a read fails or a stored value does not decode.
    pub(crate) fn find_at_tip(
        connection: &mut SqliteConnection,
        id: &str,
        tips: &Tips,
    ) -> anyhow::Result<Option<WalletIndex>> {
        let row: Option<IndexRow> = diesel::sql_query(BY_IDS)
            .bind::<Text, _>(serde_json::to_string(tips)?)
            .bind::<Text, _>(serde_json::to_string(&[id])?)
            .get_result(connection)
            .optional()?;
        let Some(row) = row else {
            return Ok(None);
        };
        let mut index = WalletIndex::try_from(row)?;
        index.legs = find_legs(connection, &[id])?.remove(id).unwrap_or_default();
        Ok(Some(index))
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
    pub(crate) fn state(code: i32) -> anyhow::Result<WalletIndexState> {
        Ok(match code {
            0 => WalletIndexState::Indexing,
            1 => WalletIndexState::Paused,
            2 => WalletIndexState::NoData,
            _ => bail!("a stored index state {code} is unknown"),
        })
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
        diesel::insert_into(wallet_index::table)
            .values(&row)
            .on_conflict_do_nothing()
            .execute(connection)?;
        Ok(())
    }

    /// How many distinct wallets of every network are indexed.
    pub(crate) fn count_addresses(connection: &mut SqliteConnection) -> QueryResult<i64> {
        wallet_index::table
            .select(sql::<BigInt>("COUNT(DISTINCT wallet_id)"))
            .get_result(connection)
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
    /// Which of `ids` are kept indexes.
    pub(crate) fn find_ids_in(
        connection: &mut SqliteConnection,
        ids: &[&str],
    ) -> QueryResult<std::collections::BTreeSet<String>> {
        wallet_index::table
            .filter(wallet_index::id.eq_any(ids))
            .select(wallet_index::id)
            .load::<String>(connection)
            .map(|found| found.into_iter().collect())
    }

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

    /// The fully indexed indexes of `network` that `selector` picks:
    /// indexed through a live block at or above `confirmed` (the universe, so
    /// `Not` is relative to it), balances at `confirmed` (as [`find_by_ids`]).
    ///
    /// # Errors
    ///
    /// As [`find_by_ids`].
    pub(crate) fn select(
        connection: &mut SqliteConnection,
        network: &Network,
        selector: &Selector,
        confirmed: i64,
    ) -> anyhow::Result<Selection> {
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
        let ids: Vec<String> = wallet_index::table
            .inner_join(wallet::table)
            .filter(wallet::chain.eq(network.chain.as_str()))
            .filter(wallet::network.eq(&network.name))
            .filter(wallet_index::indexed_at.eq_any(live))
            .filter(wallet_index::id.eq_any(picked))
            .select(wallet_index::id)
            .load(connection)?;
        find_by_ids(connection, network, &ids, confirmed)
    }

    /// The indexes `ids` of `network` (kept ones; fully indexed or not),
    /// balances at final block `confirmed`, each with its pending legs (what
    /// [`WalletIndex::spendable`] reads; not its mined history): one query
    /// for the indexes, one for the pending legs.
    ///
    /// # Errors
    ///
    /// When a read fails, a stored value does not decode, a confirmed
    /// balance is negative, or an index is of another network.
    pub(crate) fn find_by_ids(
        connection: &mut SqliteConnection,
        network: &Network,
        ids: &[String],
        confirmed: i64,
    ) -> anyhow::Result<Selection> {
        let tips = Tips(std::collections::BTreeMap::from([(
            format!("{}.{}", network.chain, network.name),
            Tip {
                tip: None,
                confirmed: Some(uint(confirmed)?),
            },
        )]));
        let indexes = decode(
            diesel::sql_query(BY_IDS)
                .bind::<Text, _>(serde_json::to_string(&tips)?)
                .bind::<Text, _>(serde_json::to_string(ids)?)
                .load(connection)?,
        )?;
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let mut pending = find_pending_legs(connection, &refs)?;
        let mut found = Selection::new(network.clone(), confirmed);
        for mut index in indexes {
            index.legs = pending.remove(&index.id).unwrap_or_default();
            let negative = index
                .balance_confirmed
                .as_deref()
                .is_some_and(|total| total.starts_with('-'));
            ensure!(!negative, "a confirmed balance is negative");
            found.insert(index)?;
        }
        Ok(found)
    }

    /// What `target` names on `this`'s network at its final block, with
    /// `this` as `This`; `None` when a sibling is not fully indexed.
    ///
    /// # Errors
    ///
    /// As [`find_by_ids`].
    pub(crate) fn find_by_target(
        connection: &mut SqliteConnection,
        this: &Selection,
        target: &Target,
    ) -> anyhow::Result<Option<Selection>> {
        let network = this.network();
        let confirmed = this.confirmed_at();
        Ok(match target {
            Target::This(This { asset: None }) => Some(this.clone()),
            Target::This(This { asset: Some(asset) }) => {
                let ids = this
                    .values()
                    .map(|one| one.sibling_id(asset))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let found = select(connection, network, &Selector::Ids(ids.clone()), confirmed)?;
                (found.len() == ids.len()).then_some(found)
            }
            Target::Select(selector) => Some(select(connection, network, selector, confirmed)?),
        })
    }
}
