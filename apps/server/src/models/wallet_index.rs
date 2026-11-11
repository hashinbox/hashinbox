//! `wallet_index`: one asset of one address being indexed, with its
//! birthday and the block it is indexed through.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, bail, ensure};
use base::{Address, Addressed, Asset, BlockHeader, Decimal, Network, Networked, WalletIndexState};
use diesel::{
    dsl::sql,
    prelude::*,
    sql_types::{BigInt, Nullable, Text},
    sqlite::Sqlite,
};
use serde::{Deserialize, Serialize};

use super::{
    Block, Wallet, child, int,
    schema::{block, wallet, wallet_index, wallet_index_with_balance},
    trigger::{Balance, Target, This},
    uint,
    wallet_index_selector::Selector,
};

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

/// Serializes an asset as its id.
fn asset_id<S: serde::Serializer>(asset: &Asset, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&asset.id)
}

/// One asset of one wallet being indexed, as every read answers it (the
/// API shows it as is): its columns, how far it is indexed against its
/// network's tip, its balances and its pending legs. All computed in SQL
/// (`wallet_index_with_balance`); amounts are exact atomic decimal text, never summed.
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
    /// The network's final (confirmed) block: its indexer's, or a trigger
    /// round's; null when unknown.
    pub(crate) finality_at: Option<i64>,
    /// Its confirmed balance: its last live mined leg's total at or below
    /// `finality_at`, `0` without one; negative when indexed debits exceed
    /// indexed credits (history before the start is unknown); null while
    /// `finality_at` is unknown.
    pub(crate) balance_confirmed: Option<String>,
    /// What it had received by then (that leg's `total_received`):
    /// `balance_confirmed = balance_confirmed_received -
    /// balance_confirmed_sent`.
    pub(crate) balance_confirmed_received: Option<String>,
    /// What it had sent by then (fees included), the same way.
    pub(crate) balance_confirmed_sent: Option<String>,
    /// What it can spend: `balance_confirmed` less its outgoing legs not yet
    /// in it (pending, or mined above `finality_at`); null while
    /// `balance_confirmed` is.
    pub(crate) balance_confirmed_spendable: Option<String>,
    /// What it will hold once every pending transaction lands: its newest
    /// live mined leg's total, any block (`0` without one), with what this
    /// server sent and is not mined yet, in and out.
    pub(crate) balance_pending: String,
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
    /// What it can spend: `balance_confirmed_spendable`, zero while unknown. What
    /// pending sends bring counts once confirmed.
    ///
    /// # Errors
    ///
    /// When a stored amount does not decode.
    pub(crate) fn spendable(&self) -> anyhow::Result<Decimal> {
        self.balance_confirmed_spendable
            .as_deref()
            .unwrap_or("0")
            .parse()
            .context("a stored balance does not decode")
    }

    /// The balance a trigger reads as `balance`: [`Self::spendable`], or what
    /// it will hold once every pending transaction lands.
    ///
    /// # Errors
    ///
    /// When a stored amount does not decode.
    pub(crate) fn held(&self, balance: Balance) -> anyhow::Result<Decimal> {
        match balance {
            Balance::Confirmed => self.spendable(),
            Balance::Pending => self
                .balance_pending
                .parse()
                .context("a stored balance does not decode"),
        }
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

    /// `one` alone, of the same network at the same block.
    ///
    /// # Errors
    ///
    /// When it is of another network.
    pub(crate) fn only(&self, one: &WalletIndex) -> anyhow::Result<Self> {
        let mut alone = Self::new(self.network.clone(), self.confirmed);
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
    #[derive(QueryableByName, Deserialize)]
    #[diesel(table_name = wallet_index_with_balance)]
    pub(crate) struct IndexRow {
        id: String,
        wallet_id: String,
        asset: String,
        requested_from: i64,
        birthday: i64,
        indexed_at: Option<String>,
        state: i32,
        reason: Option<String>,
        address: String,
        indexed_through: Option<i64>,
        #[diesel(sql_type = Nullable<BigInt>)]
        behind_tip: Option<i64>,
        #[diesel(sql_type = Nullable<BigInt>)]
        finality_at: Option<i64>,
        balance_confirmed: Option<String>,
        balance_confirmed_received: Option<String>,
        balance_confirmed_sent: Option<String>,
        balance_confirmed_spendable: Option<String>,
        balance_pending: String,
        keyed: bool,
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
                balance_confirmed: row.balance_confirmed,
                balance_confirmed_received: row.balance_confirmed_received,
                balance_confirmed_sent: row.balance_confirmed_sent,
                balance_confirmed_spendable: row.balance_confirmed_spendable,
                balance_pending: row.balance_pending,
                keyed: row.keyed,
                indexed_at: row.indexed_at,
            })
        }
    }

    /// Index `id` (`{wallet_id}.{asset}`) of `network`, if kept, as the
    /// wallet reads measure it (against its network's `network` row).
    ///
    /// # Errors
    ///
    /// When the read fails or a stored value does not decode.
    pub(crate) fn find_at_tip(
        connection: &mut SqliteConnection,
        network: &Network,
        id: &str,
    ) -> anyhow::Result<Option<WalletIndex>> {
        diesel::sql_query(
            "
            SELECT
                *
            FROM wallet_index_with_balance
            WHERE id = ?1",
        )
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

    /// Of `wanted` (address, asset), those whose address is a keyed wallet
    /// holding the asset now: a positive balance, pending legs counted. One
    /// read, whatever their networks.
    pub(crate) fn find_held<'a>(
        connection: &mut SqliteConnection,
        wanted: impl IntoIterator<Item = (&'a Address, &'a Asset)>,
    ) -> QueryResult<Vec<(Address, Asset)>> {
        use diesel::sql_types::Text;

        #[derive(QueryableByName)]
        struct Held {
            #[diesel(sql_type = Text)]
            chain: String,
            #[diesel(sql_type = Text)]
            network: String,
            #[diesel(sql_type = Text)]
            address: String,
            #[diesel(sql_type = Text)]
            asset: String,
        }

        let wanted: Vec<[&str; 4]> = wanted
            .into_iter()
            .map(|(address, asset)| {
                let network = &address.network;
                [
                    network.chain.as_str(),
                    &network.name,
                    &address.value,
                    &asset.id,
                ]
            })
            .collect();
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let wanted = serde_json::to_string(&wanted)
            .map_err(|error| diesel::result::Error::SerializationError(Box::new(error)))?;
        let held: Vec<Held> = diesel::sql_query(
            "SELECT chain, network, address, asset
             FROM wallet_index_with_balance
             WHERE (chain, network, address, asset) IN (
                SELECT value ->> 0, value ->> 1, value ->> 2, value ->> 3
                FROM json_each(?1)
               )
               AND keyed
               AND balance_pending != '0' AND balance_pending NOT LIKE '-%'",
        )
        .bind::<Text, _>(wanted)
        .load(connection)?;
        Ok(held
            .into_iter()
            .map(|held| {
                let network = Network::new(base::Chain::new(held.chain), held.network);
                let asset = Asset {
                    network: network.clone(),
                    id: held.asset,
                };
                (Address::new(network, held.address), asset)
            })
            .collect())
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
    /// indexed through a live block at or above `confirmed`, the network's
    /// confirmed position (the universe, so `Not` is relative to it), with
    /// the view's balances there (what [`WalletIndex::spendable`] reads), in
    /// one query.
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
    /// with the view's balances at the confirmed position, in one query.
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
            "wi.indexed_through >= ?"
        } else {
            "1"
        };
        // The view's row: its balances at the confirmed position, what a
        // round decides on.
        let mut query = diesel::sql_query(format!(
            "
            SELECT
                wi.id,
                wi.wallet_id,
                wi.asset,
                wi.requested_from,
                wi.birthday,
                wi.indexed_at,
                wi.state,
                wi.reason,
                wi.address,
                wi.keyed,
                wi.indexed_through,
                NULL AS behind_tip,
                wi.finality_at,
                wi.balance_confirmed,
                wi.balance_confirmed_received,
                wi.balance_confirmed_sent,
                wi.balance_confirmed_spendable,
                wi.balance_pending
            FROM wallet_index_with_balance wi
            WHERE wi.chain = ?
              AND wi.network = ?
              AND ({indexed})
              AND ({picked})
            ORDER BY wi.wallet_id, wi.asset"
        ))
        .into_boxed()
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
