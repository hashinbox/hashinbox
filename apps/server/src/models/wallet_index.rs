//! `wallet_index`: one asset of one address being indexed, with its
//! birthday and the block it is indexed through.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, bail, ensure};
use base::{Address, Addressed, Asset, BlockHeader, Decimal, Network, Networked, WalletIndexState};
use diesel::{
    dsl::sql,
    prelude::*,
    sql_types::{BigInt, Bool, Integer, Nullable, Text},
    sqlite::Sqlite,
};
use serde::{Deserialize, Serialize};

use super::{
    Block, Json, Leg, Wallet, child, int,
    schema::{block, wallet, wallet_index},
    trigger::{Target, This},
    uint,
    wallet::Tips,
};

/// A set of wallet indexes; resolved in SQL through [`Selector::sql`].
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

impl Selector {
    /// It as one SQL condition over `wi` (`wallet_index`) and `w` (its
    /// `wallet`), its values pushed to `binds` in placeholder order: `All`
    /// is AND (empty: true), `Any` OR (empty: false), `Not` NOT.
    pub(crate) fn sql(&self, binds: &mut Vec<String>) -> String {
        let group = |many: &[Self], binds: &mut Vec<String>, join: &str, empty: &str| {
            if many.is_empty() {
                return empty.to_owned();
            }
            let parts: Vec<String> = many.iter().map(|one| one.sql(binds)).collect();
            format!("({})", parts.join(join))
        };
        match self {
            Self::Ids(ids) => {
                binds.extend(ids.iter().cloned());
                format!("wi.id IN ({})", vec!["?"; ids.len()].join(", "))
            }
            Self::Asset(asset) => {
                binds.push(asset.clone());
                "wi.asset = ?".to_owned()
            }
            Self::Tag(name) => {
                binds.push(name.clone());
                "EXISTS (SELECT 1 FROM json_each(w.tags) j WHERE j.value = ?)".to_owned()
            }
            Self::Untagged {} => "json_array_length(w.tags) = 0".to_owned(),
            Self::All(all) => group(all, binds, " AND ", "1"),
            Self::Any(any) => group(any, binds, " OR ", "0"),
            Self::Not(inner) => format!("NOT ({})", inner.sql(binds)),
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
/// network's tip, its balances and its pending legs. All computed in SQL
/// (`db::BY_IDS`); amounts are exact atomic decimal text, never summed.
/// Positions are native (height or slot).
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
    /// Its pending legs (no block, no total), by id; its mined ones are
    /// paged by `leg::db::find_page`.
    pub(crate) legs: Vec<Leg>,
    /// Whether its wallet's key is kept here.
    #[serde(skip)]
    pub(crate) keyed: bool,
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

    /// A [`WalletIndex`] as read, of its wallet's network (not a column):
    /// a row of [`find_at_tip`] or [`select`], or one object of a wallet read's
    /// `indexes` (SQLite builds the same fields as JSON).
    #[derive(Queryable, QueryableByName, Deserialize)]
    pub(crate) struct IndexRow {
        #[diesel(sql_type = Text)]
        id: String,
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
        #[diesel(sql_type = Bool)]
        keyed: bool,
        #[diesel(sql_type = Text, deserialize_as = Json<Vec<Leg>>)]
        legs: Vec<Leg>,
    }

    /// The row of an index of `network`.
    impl TryFrom<(&Network, IndexRow)> for WalletIndex {
        type Error = anyhow::Error;

        fn try_from((network, row): (&Network, IndexRow)) -> anyhow::Result<Self> {
            Ok(Self {
                id: row.id,
                wallet_id: row.wallet_id,
                address: Address::new(network.clone(), row.address),
                asset: Asset {
                    network: network.clone(),
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
                legs: row.legs,
                keyed: row.keyed,
                indexed_at: row.indexed_at,
            })
        }
    }

    /// Index `id` (`{wallet_id}.{asset}`) of `network`, if kept, measured
    /// against `tips` as the wallet reads measure it, with its pending legs.
    ///
    /// # Errors
    ///
    /// When the read fails or a stored value does not decode.
    pub(crate) fn find_at_tip(
        connection: &mut SqliteConnection,
        network: &Network,
        id: &str,
        tips: &Tips,
    ) -> anyhow::Result<Option<WalletIndex>> {
        // Index `?2`, measured against its network's entry in tips `?1` (JSON:
        // tip and final block), with its pending legs. Each balance is the
        // total of the index's last live mined leg (at or below the final block
        // for the confirmed one).
        diesel::sql_query(
            r#"
            WITH t AS (
                SELECT
                    key AS network_id,
                    json_extract(value, '$.tip') AS tip,
                    json_extract(value, '$.confirmed') AS confirmed
                FROM json_each(?1)
            )
            SELECT
                wi.id,
                wi.wallet_id,
                wi.asset,
                wi.requested_from,
                wi.birthday,
                wi.indexed_at,
                wi.state,
                wi.reason,
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
                END AS balance_confirmed,
                w.secret IS NOT NULL AS keyed,
                COALESCE((
                    SELECT json_group_array(
                        json(l.json)
                        ORDER BY l.id
                    )
                    FROM leg_json l
                    WHERE l.wallet_index_id = wi.id
                      AND l.block IS NULL
                ), '[]') AS legs
            FROM wallet_index wi
            JOIN wallet w ON w.id = wi.wallet_id
            LEFT JOIN block b ON b.id = wi.indexed_at
            LEFT JOIN t ON t.network_id = w.chain || '.' || w.network
            WHERE wi.id = ?2"#,
        )
        .bind::<Text, _>(serde_json::to_string(tips)?)
        .bind::<Text, _>(id)
        .get_result::<IndexRow>(connection)
        .optional()?
        .map(|row| WalletIndex::try_from((network, row)))
        .transpose()
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

    /// An index's id.
    #[derive(QueryableByName)]
    struct IndexId {
        #[diesel(sql_type = Text)]
        id: String,
    }

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
        let rows: Vec<(Row, String, Option<crate::models::block::db::Row>)> = wallet_index::table
            .inner_join(wallet::table)
            .left_join(block::table)
            .filter(wallet::chain.eq(network.chain.as_str()))
            .filter(wallet::network.eq(&network.name))
            .select((
                Row::as_select(),
                wallet::address,
                Option::<crate::models::block::db::Row>::as_select(),
            ))
            .order((wallet::address, wallet_index::asset))
            .load(connection)?;
        rows.into_iter()
            .map(|(row, address, block)| {
                let block = block.map(Block::from);
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
        // The lowest next position of a network's indexing (state 0) indexes,
        // at or below a bound (NULL: none).
        let row: Next = diesel::sql_query(
            "
            SELECT
                MIN(n) AS next
            FROM (
                SELECT
                    CASE
                        WHEN wi.indexed_at IS NULL THEN wi.birthday
                        ELSE b.position + 1
                    END AS n
                FROM wallet_index wi
                JOIN wallet w ON w.id = wi.wallet_id
                LEFT JOIN block b ON b.id = wi.indexed_at
                WHERE w.chain = ?1
                  AND w.network = ?2
                  AND wi.state = 0
            )
            WHERE ?3 IS NULL
               OR n <= ?3",
        )
        .bind::<Text, _>(network.chain.as_str())
        .bind::<Text, _>(&network.name)
        .bind::<Nullable<BigInt>, _>(through)
        .get_result(connection)?;
        Ok(row.next)
    }

    /// Moves every index `block` (kept live, its parent's id `parent`)
    /// applies to onto it; their ids.
    pub(crate) fn advance(
        connection: &mut SqliteConnection,
        block: &Block,
        parent: Option<&str>,
    ) -> QueryResult<BTreeSet<String>> {
        // Moves every indexing (state 0) index of network `?1`, `?2` that the
        // block at position `?3` (id `?5`) applies to onto it, and answers
        // their ids: birthday at or below it, and indexed through its parent
        // `?4`, or unset with no live block from the birthday up to it.
        let rows: Vec<IndexId> = diesel::sql_query(
            "
            UPDATE wallet_index
            SET indexed_at = ?5
            WHERE state = 0
              AND birthday <= ?3
              AND wallet_id IN (
                  SELECT id
                  FROM wallet
                  WHERE chain = ?1
                    AND network = ?2
              )
              AND (
                  indexed_at = ?4
                  OR (
                      indexed_at IS NULL
                      AND NOT EXISTS (
                          SELECT 1
                          FROM block b
                          WHERE b.chain = ?1
                            AND b.network = ?2
                            AND b.archived_at IS NULL
                            AND b.position >= wallet_index.birthday
                            AND b.position < ?3
                      )
                  )
              )
            RETURNING id",
        )
        .bind::<Text, _>(&block.chain)
        .bind::<Text, _>(&block.network)
        .bind::<BigInt, _>(block.position)
        .bind::<Nullable<Text>, _>(parent)
        .bind::<Text, _>(&block.id)
        .load(connection)?;
        Ok(rows.into_iter().map(|row| row.id).collect())
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
        // Moves every index indexed through a live block of a network at or
        // above a position back to a block (or unset when it is below the
        // birthday). An index's block is of its own network.
        diesel::sql_query(
            "
            UPDATE wallet_index
            SET indexed_at = CASE
                WHEN ?4 >= birthday THEN ?5
                ELSE NULL
            END
            WHERE indexed_at IN (
                SELECT id
                FROM block
                WHERE chain = ?1
                  AND network = ?2
                  AND archived_at IS NULL
                  AND position >= ?3
            )",
        )
        .bind::<Text, _>(network.chain.as_str())
        .bind::<Text, _>(&network.name)
        .bind::<BigInt, _>(from)
        .bind::<Nullable<BigInt>, _>(below.map(|block| block.position))
        .bind::<Nullable<Text>, _>(below.map(|block| block.id.as_str()))
        .execute(connection)
    }

    /// The fully indexed indexes of `network` that `selector` picks:
    /// indexed through a live block at or above `confirmed` (the universe, so
    /// `Not` is relative to it), balances at `confirmed`, each with its
    /// pending legs (what [`WalletIndex::spendable`] reads), in one query.
    ///
    /// # Errors
    ///
    /// When a read fails, a stored value does not decode or a confirmed
    /// balance is negative.
    pub(crate) fn select(
        connection: &mut SqliteConnection,
        network: &Network,
        selector: &Selector,
        confirmed: i64,
    ) -> anyhow::Result<Selection> {
        read(connection, network, selector, confirmed, true)
    }

    /// The indexes `ids` of `network` (kept ones; fully indexed or not), as
    /// [`select`] reads them.
    ///
    /// # Errors
    ///
    /// As [`select`].
    #[cfg(test)]
    pub(crate) fn find_by_ids(
        connection: &mut SqliteConnection,
        network: &Network,
        ids: &[String],
        confirmed: i64,
    ) -> anyhow::Result<Selection> {
        read(
            connection,
            network,
            &Selector::Ids(ids.to_vec()),
            confirmed,
            false,
        )
    }

    /// The indexes of `network` that `selector` picks, only those indexed
    /// through a live block at or above `confirmed` when `fully_indexed`,
    /// balances at `confirmed`, each with its pending legs, in one query.
    fn read(
        connection: &mut SqliteConnection,
        network: &Network,
        selector: &Selector,
        confirmed: i64,
        fully_indexed: bool,
    ) -> anyhow::Result<Selection> {
        let mut binds = Vec::new();
        let picked = selector.sql(&mut binds);
        let indexed = if fully_indexed {
            "b.archived_at IS NULL AND b.position >= ?"
        } else {
            "1"
        };
        // Each balance is the total of the index's last live mined leg ('0':
        // none), the confirmed one at or below `confirmed`.
        let mut query = diesel::sql_query(format!(
            r#"
            SELECT
                wi.id,
                wi.wallet_id,
                wi.asset,
                wi.requested_from,
                wi.birthday,
                wi.indexed_at,
                wi.state,
                wi.reason,
                w.address,
                b.position AS indexed_through,
                NULL AS behind_tip,
                ? AS finality_at,
                COALESCE((
                    SELECT l.total
                    FROM leg l
                    JOIN block lb ON lb.id = l.block
                    WHERE l.wallet_index_id = wi.id
                      AND lb.archived_at IS NULL
                    ORDER BY l.position DESC, l.seq DESC
                    LIMIT 1
                ), '0') AS balance,
                COALESCE((
                    SELECT l.total
                    FROM leg l
                    JOIN block lb ON lb.id = l.block
                    WHERE l.wallet_index_id = wi.id
                      AND lb.archived_at IS NULL
                      AND l.position <= ?
                    ORDER BY l.position DESC, l.seq DESC
                    LIMIT 1
                ), '0') AS balance_confirmed,
                w.secret IS NOT NULL AS keyed,
                COALESCE((
                    SELECT json_group_array(
                        json(l.json)
                        ORDER BY l.id
                    )
                    FROM leg_json l
                    WHERE l.wallet_index_id = wi.id
                      AND l.block IS NULL
                ), '[]') AS legs
            FROM wallet_index wi
            JOIN wallet w ON w.id = wi.wallet_id
            LEFT JOIN block b ON b.id = wi.indexed_at
            WHERE w.chain = ?
              AND w.network = ?
              AND ({indexed})
              AND ({picked})
            ORDER BY wi.wallet_id, wi.asset"#
        ))
        .into_boxed()
        .bind::<BigInt, _>(confirmed)
        .bind::<BigInt, _>(confirmed)
        .bind::<Text, _>(network.chain.as_str().to_owned())
        .bind::<Text, _>(network.name.clone());
        if fully_indexed {
            query = query.bind::<BigInt, _>(confirmed);
        }
        for value in binds {
            query = query.bind::<Text, _>(value);
        }
        let rows: Vec<IndexRow> = query.load(connection)?;
        let mut found = Selection::new(network.clone(), confirmed);
        for row in rows {
            let index = WalletIndex::try_from((network, row))?;
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
    /// `this` as `This`, in one query; `None` when a sibling is not fully
    /// indexed.
    ///
    /// # Errors
    ///
    /// As [`select`].
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
                // The `asset` index of each wallet of `this`, once.
                let siblings: BTreeSet<String> = this
                    .values()
                    .map(|one| format!("{}.{asset}", one.wallet_id))
                    .collect();
                let wanted = siblings.len();
                let picked = Selector::Ids(siblings.into_iter().collect());
                let found = read(connection, network, &picked, confirmed, true)?;
                // Fewer fully indexed than wallets: a sibling is missing.
                (found.len() == wanted).then_some(found)
            }
            Target::Select(selector) => Some(select(connection, network, selector, confirmed)?),
        })
    }
}
