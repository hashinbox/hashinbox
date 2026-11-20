//! `wallet_index`: one asset of one address being indexed, with its
//! birthday and the position it is indexed through.

use std::collections::BTreeSet;

use ::trigger::selection::{Index, Selector};
use anyhow::{Context as _, bail, ensure};
use base::{Address, Addressed, Asset, Chain, Decimal, Network, Networked, Progress};
use diesel::{
    deserialize::FromSqlRow,
    dsl::sql,
    expression::AsExpression,
    prelude::*,
    sql_types::{BigInt, Nullable, Text},
    sqlite::Sqlite,
};
use serde::{Deserialize, Serialize};

use super::{
    Block, Id, IdError,
    id::typed_id,
    int,
    schema::{wallet, wallet_index, wallet_index_with_balance},
    uint,
    wallet::WalletId,
};

/// A wallet index's id: its wallet's id and the asset, `{chain}.{network}.{address}.{asset}`.
#[derive(Clone, Debug, PartialEq, Eq, AsExpression, FromSqlRow)]
#[diesel(sql_type = Text)]
pub(crate) struct WalletIndexId {
    pub(crate) wallet: WalletId,
    pub(crate) asset: String,
    id: Id,
}

impl WalletIndexId {
    /// The id of the index of `asset` of `address`.
    ///
    /// # Errors
    ///
    /// When a part holds a dot.
    pub(crate) fn of(address: &Address, asset: &Asset) -> Result<Self, IdError> {
        let wallet = WalletId::of(address)?;
        let id = Id::child(wallet.id(), &[&asset.id])?;
        Ok(Self {
            wallet,
            asset: asset.id.clone(),
            id,
        })
    }

    /// `text` as a wallet index id.
    ///
    /// # Errors
    ///
    /// Unless it has its four parts.
    pub(crate) fn parse(text: &str) -> Result<Self, IdError> {
        let shape = "{chain}.{network}.{address}.{asset}";
        let [chain, network, address, asset] = Id::parts::<4>(text, shape)?;
        let network = Network::new(Chain::new(chain), network);
        let asset = Asset {
            network: network.clone(),
            id: asset.to_owned(),
        };
        Self::of(&Address::new(network, address), &asset)
    }
}

typed_id!(WalletIndexId, wallet.address.network);

impl Addressed for WalletIndexId {
    fn address(&self) -> &Address {
        self.wallet.address()
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

/// A wallet index's state.
impl From<base::IndexState> for IndexState {
    fn from(state: base::IndexState) -> Self {
        match state {
            base::IndexState::Indexing => Self::Indexing,
            base::IndexState::Paused => Self::Paused,
            base::IndexState::NoData => Self::NoData,
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
    #[schema(value_type = String)]
    pub(crate) id: WalletIndexId,
    /// `{chain}.{network}.{address}`.
    #[schema(value_type = String)]
    pub(crate) wallet_id: WalletId,
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
    /// indexed credits (history before the start or a jump is unknown);
    /// null while `finality_at` is unknown.
    pub(crate) balance_confirmed: Option<String>,
    /// What it had received by then (that leg's `total_received`):
    /// `balance_confirmed` is `balance_confirmed_received -
    /// balance_confirmed_sent` plus its adjustments.
    pub(crate) balance_confirmed_received: Option<String>,
    /// What it had sent by then (fees included), the same way.
    pub(crate) balance_confirmed_sent: Option<String>,
    /// What it can spend: `balance_confirmed` less its outgoing legs not yet
    /// in it (pending, or mined above `finality_at`); null while
    /// `balance_confirmed` is.
    pub(crate) balance_confirmed_spendable: Option<String>,
    /// What it will hold once every pending transaction lands: its newest
    /// live mined leg's total, any block (`0` without one), with what this
    /// server sent and is not mined yet, in and out; null before its first
    /// block (`indexed_through`), as every balance is.
    pub(crate) balance_pending: Option<String>,
    /// Whether its wallet's key is kept here.
    #[serde(skip)]
    pub(crate) keyed: bool,
    /// The position it is indexed through.
    #[serde(skip)]
    pub(crate) indexed_at: Option<i64>,
}

impl Networked for WalletIndex {
    fn network(&self) -> &Network {
        self.id.network()
    }
}

impl Addressed for WalletIndex {
    fn address(&self) -> &Address {
        self.id.address()
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

    /// It as a trigger reads it: what it can spend ([`Self::spendable`]),
    /// and what it will hold once every pending transaction lands.
    ///
    /// # Errors
    ///
    /// When a stored amount does not decode.
    pub(crate) fn index(self) -> anyhow::Result<Index> {
        Ok(Index {
            confirmed: self.spendable()?,
            pending: self
                .balance_pending
                .as_deref()
                .unwrap_or("0")
                .parse()
                .context("a stored balance does not decode")?,
            address: self.id.address().clone(),
            id: self.id.into(),
            asset: self.asset,
            keyed: self.keyed,
        })
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
        id: WalletIndexId,
        wallet_id: WalletId,
        asset: String,
        requested_from: i64,
        birthday: i64,
        indexed_at: Option<i64>,
        state: i32,
        reason: Option<String>,
    }

    impl Row {
        /// The index.
        ///
        /// # Errors
        ///
        /// When a stored value does not decode.
        pub(crate) fn progress(self) -> anyhow::Result<base::WalletIndex> {
            Ok(base::WalletIndex {
                asset: Asset {
                    network: self.id.network().clone(),
                    id: self.asset,
                },
                address: self.id.address().clone(),
                requested_from: uint(self.requested_from)?,
                birthday: uint(self.birthday)?,
                progress: Progress {
                    indexed_at: self.indexed_at.map(uint).transpose()?,
                    state: state(self.state)?,
                    reason: self.reason,
                },
            })
        }
    }

    /// A [`WalletIndex`] as read: a row of [`find_at_tip`] or [`select`], or
    /// one object of a wallet read's `indexes` (SQLite builds the same fields
    /// as JSON).
    #[derive(QueryableByName, Deserialize)]
    #[diesel(table_name = wallet_index_with_balance)]
    pub(crate) struct IndexRow {
        id: WalletIndexId,
        wallet_id: WalletId,
        asset: String,
        requested_from: i64,
        birthday: i64,
        indexed_at: Option<i64>,
        state: i32,
        reason: Option<String>,
        indexed_through: Option<i64>,
        #[diesel(sql_type = Nullable<BigInt>)]
        behind_tip: Option<i64>,
        #[diesel(sql_type = Nullable<BigInt>)]
        finality_at: Option<i64>,
        balance_confirmed: Option<String>,
        balance_confirmed_received: Option<String>,
        balance_confirmed_sent: Option<String>,
        balance_confirmed_spendable: Option<String>,
        balance_pending: Option<String>,
        keyed: bool,
    }

    /// The row as its index.
    impl TryFrom<IndexRow> for WalletIndex {
        type Error = anyhow::Error;

        fn try_from(row: IndexRow) -> anyhow::Result<Self> {
            Ok(Self {
                asset: Asset {
                    network: row.id.network().clone(),
                    id: row.asset,
                },
                id: row.id,
                wallet_id: row.wallet_id,
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

    /// Index `id` (`{wallet_id}.{asset}`), if kept, as the wallet reads
    /// measure it (against its network's `network` row).
    ///
    /// # Errors
    ///
    /// When the read fails or a stored value does not decode.
    pub(crate) fn find_at_tip(
        connection: &mut SqliteConnection,
        id: impl AsRef<str>,
    ) -> anyhow::Result<Option<WalletIndex>> {
        diesel::sql_query(
            "
            SELECT
                *
            FROM wallet_index_with_balance
            WHERE id = ?1",
        )
        .bind::<Text, _>(id.as_ref())
        .get_result::<IndexRow>(connection)
        .optional()?
        .map(WalletIndex::try_from)
        .transpose()
    }

    /// `state` as stored, a transaction index's too.
    pub(crate) const fn code(state: base::IndexState) -> i32 {
        match state {
            base::IndexState::Indexing => 0,
            base::IndexState::Paused => 1,
            base::IndexState::NoData => 2,
        }
    }

    /// A stored `state`, a transaction index's too.
    ///
    /// # Errors
    ///
    /// For a code no writer stores.
    pub(crate) fn state(code: i32) -> anyhow::Result<base::IndexState> {
        Ok(match code {
            0 => base::IndexState::Indexing,
            1 => base::IndexState::Paused,
            2 => base::IndexState::NoData,
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
        let id = WalletIndexId::of(&index.address, &index.asset)?;
        let row = Row {
            wallet_id: id.wallet.clone(),
            id,
            asset: index.asset.id.clone(),
            requested_from: int(index.requested_from)?,
            birthday: int(index.birthday)?,
            indexed_at: None,
            state: code(index.progress.state),
            reason: index.progress.reason.clone(),
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

    /// Every index of `network`.
    ///
    /// # Errors
    ///
    /// When the read fails or a stored value does not decode.
    pub(crate) fn find_all(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> anyhow::Result<Vec<base::WalletIndex>> {
        let rows: Vec<Row> = wallet_index::table
            .inner_join(wallet::table)
            .filter(wallet::chain.eq(network.chain.as_str()))
            .filter(wallet::network.eq(&network.name))
            .select(Row::as_select())
            .order((wallet::address, wallet_index::asset))
            .load(connection)?;
        rows.into_iter().map(Row::progress).collect()
    }

    /// An index's next position: the one after the position it is indexed
    /// through, or its birthday while it has not started.
    const NEXT: &str = "COALESCE(wallet_index.indexed_at + 1, wallet_index.birthday)";

    /// `network`'s indexing indexes whose next position is below `below`,
    /// furthest behind first, at most `limit`.
    ///
    /// # Errors
    ///
    /// When the read fails or a stored value does not decode.
    pub(crate) fn find_behind(
        connection: &mut SqliteConnection,
        network: &Network,
        below: i64,
        limit: i64,
    ) -> anyhow::Result<Vec<base::WalletIndex>> {
        let rows: Vec<Row> = wallet_index::table
            .inner_join(wallet::table)
            .filter(wallet::chain.eq(network.chain.as_str()))
            .filter(wallet::network.eq(&network.name))
            .filter(wallet_index::state.eq(code(base::IndexState::Indexing)))
            .filter(sql::<BigInt>(NEXT).lt(below))
            .select(Row::as_select())
            .order((sql::<BigInt>(NEXT), wallet::address, wallet_index::asset))
            .limit(limit)
            .load(connection)?;
        rows.into_iter().map(Row::progress).collect()
    }

    /// The lowest next position of `network`'s indexes at or above `from`
    /// and at or below `through` (`None`: no upper bound), in one query.
    pub(crate) fn find_next(
        connection: &mut SqliteConnection,
        network: &Network,
        from: i64,
        through: Option<i64>,
    ) -> QueryResult<Option<i64>> {
        // The lowest next position of a network's indexing (state 0)
        // indexes, at or above a bound and at or below another (NULL: none).
        let row: Next = diesel::sql_query(
            "
            SELECT
                MIN(n) AS next
            FROM (
                SELECT
                    COALESCE(wi.indexed_at + 1, wi.birthday) AS n
                FROM wallet_index wi
                JOIN wallet w ON w.id = wi.wallet_id
                WHERE w.chain = ?1
                  AND w.network = ?2
                  AND wi.state = 0
            )
            WHERE n >= ?3
              AND (?4 IS NULL OR n <= ?4)",
        )
        .bind::<Text, _>(network.chain.as_str())
        .bind::<Text, _>(&network.name)
        .bind::<BigInt, _>(from)
        .bind::<Nullable<BigInt>, _>(through)
        .get_result(connection)?;
        Ok(row.next)
    }

    /// Moves every index `block` (kept) applies to onto its position;
    /// their ids.
    pub(crate) fn advance(
        connection: &mut SqliteConnection,
        block: &Block,
    ) -> QueryResult<BTreeSet<String>> {
        // Moves every indexing (state 0) index of network `?1`, `?2` that
        // the block at position `?3` (its parent at position `?4`) applies
        // to onto it, and answers their ids: birthday at or below it, and
        // indexed through its parent's position (nothing lies between the
        // two, so any position from the parent's up to the block's: a
        // skipped slot, a settle or a reorg's cap no block sits at), or
        // unset with its parent below the birthday (none: the chain's
        // first). By the chain's links, not the kept blocks: an index never
        // skips a block nobody kept.
        let rows: Vec<IndexId> = diesel::sql_query(
            "
            UPDATE wallet_index
            SET indexed_at = ?3
            WHERE state = 0
              AND birthday <= ?3
              AND wallet_id IN (
                  SELECT id
                  FROM wallet
                  WHERE chain = ?1
                    AND network = ?2
              )
              AND (
                  (indexed_at >= ?4 AND indexed_at < ?3)
                  OR (indexed_at IS NULL AND (?4 IS NULL OR ?4 < birthday))
              )
            RETURNING id",
        )
        .bind::<Text, _>(&block.chain)
        .bind::<Text, _>(&block.network)
        .bind::<BigInt, _>(block.position)
        .bind::<Nullable<BigInt>, _>(block.parent_position)
        .load(connection)?;
        Ok(rows.into_iter().map(|row| row.id).collect())
    }

    /// Of `ids`, the indexes behind `mined`: indexed through a position
    /// below it, in any state (paused, `no_data`, or catching up). A
    /// transaction its transaction index finds in `mined` keeps its legs of
    /// them: reading `mined` again writes them again, and a settle at or
    /// above it counts them.
    pub(crate) fn find_behind_block(
        connection: &mut SqliteConnection,
        mined: &Block,
        ids: &BTreeSet<String>,
    ) -> QueryResult<BTreeSet<String>> {
        wallet_index::table
            .filter(wallet_index::id.eq_any(ids))
            .filter(wallet_index::indexed_at.lt(mined.position))
            .select(wallet_index::id)
            .load::<String>(connection)
            .map(|found| found.into_iter().collect())
    }

    /// Moves the birthday of `index` to `start` while it has not started.
    ///
    /// # Errors
    ///
    /// When a value is out of range or the write fails.
    pub(crate) fn move_start(
        connection: &mut SqliteConnection,
        index: &base::WalletIndex,
        start: u64,
    ) -> anyhow::Result<()> {
        diesel::update(
            wallet_index::table
                .filter(wallet_index::id.eq(WalletIndexId::of(&index.address, &index.asset)?))
                .filter(wallet_index::indexed_at.is_null()),
        )
        .set(wallet_index::birthday.eq(int(start)?))
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
        state: base::IndexState,
        reason: Option<&str>,
    ) -> anyhow::Result<usize> {
        let changes = (
            wallet_index::state.eq(code(state)),
            wallet_index::reason.eq(reason),
        );
        let wallet_id = WalletId::of(address)?;
        Ok(match asset {
            Some(asset) => diesel::update(
                wallet_index::table.find(Id::child(wallet_id.id(), &[asset])?.to_string()),
            )
            .set(changes)
            .execute(connection)?,
            None => diesel::update(
                wallet_index::table.filter(wallet_index::wallet_id.eq(wallet_id.as_ref())),
            )
            .set(changes)
            .execute(connection)?,
        })
    }

    /// Resumes index `id`, indexing, clearing its reason; how many (0 or 1).
    pub(crate) fn resume(
        connection: &mut SqliteConnection,
        id: impl AsRef<str>,
    ) -> QueryResult<usize> {
        diesel::update(wallet_index::table.find(id.as_ref()))
            .set((
                wallet_index::state.eq(code(base::IndexState::Indexing)),
                wallet_index::reason.eq(None::<String>),
            ))
            .execute(connection)
    }

    /// Unsets the progress of index `id` when it is indexing, its birthday
    /// at most `below`, the block under the chain's line: its next block is
    /// below the line, so the indexer settles it again. How many (0 or 1).
    pub(crate) fn reconcile(
        connection: &mut SqliteConnection,
        id: impl AsRef<str>,
        below: i64,
    ) -> QueryResult<usize> {
        diesel::update(
            wallet_index::table
                .find(id.as_ref())
                .filter(wallet_index::state.eq(code(base::IndexState::Indexing))),
        )
        .set((
            wallet_index::indexed_at.eq(None::<i64>),
            wallet_index::birthday.eq(diesel::dsl::sql::<BigInt>("MIN(birthday, ")
                .bind::<BigInt, _>(below)
                .sql(")")),
        ))
        .execute(connection)
    }

    /// Settles index `id` at `position`: it continues from the one after,
    /// its birthday there.
    pub(crate) fn settle(
        connection: &mut SqliteConnection,
        id: impl AsRef<str>,
        position: i64,
    ) -> QueryResult<usize> {
        diesel::update(wallet_index::table.find(id.as_ref()))
            .set((
                wallet_index::birthday.eq(position),
                wallet_index::indexed_at.eq(position),
            ))
            .execute(connection)
    }

    /// Restarts index `id` at `birthday` (a jump): unstarted, indexing; its
    /// `requested_from` and legs stay.
    ///
    /// # Errors
    ///
    /// When `birthday` is out of range or the write fails.
    pub(crate) fn restart(
        connection: &mut SqliteConnection,
        id: impl AsRef<str>,
        birthday: u64,
    ) -> anyhow::Result<()> {
        diesel::update(wallet_index::table.find(id.as_ref()))
            .set((
                wallet_index::birthday.eq(int(birthday)?),
                wallet_index::indexed_at.eq(None::<i64>),
                wallet_index::state.eq(code(base::IndexState::Indexing)),
            ))
            .execute(connection)?;
        Ok(())
    }

    /// Forgets index `id`; how many went (0 or 1).
    pub(crate) fn delete(
        connection: &mut SqliteConnection,
        id: impl AsRef<str>,
    ) -> QueryResult<usize> {
        diesel::delete(wallet_index::table.find(id.as_ref())).execute(connection)
    }

    /// Caps every index of `network` indexed through a position above `to`
    /// at `to` (a reorg's: `MIN(indexed_at, to)`), or unsets it when `to` is
    /// below its birthday; it reads on from the next block kept above `to`
    /// ([`advance`]). How many.
    pub(crate) fn cap_indexed_at(
        connection: &mut SqliteConnection,
        network: &Network,
        to: i64,
    ) -> QueryResult<usize> {
        diesel::sql_query(
            "
            UPDATE wallet_index
            SET indexed_at = IIF(?3 >= birthday, ?3, NULL)
            WHERE indexed_at > ?3
              AND wallet_id IN (
                  SELECT id
                  FROM wallet
                  WHERE chain = ?1
                    AND network = ?2
              )",
        )
        .bind::<Text, _>(network.chain.as_str())
        .bind::<Text, _>(&network.name)
        .bind::<BigInt, _>(to)
        .execute(connection)
    }

    /// The fully indexed indexes of `network` that `selector` picks:
    /// indexing (not paused or without data) and indexed
    /// through a position at or above `confirmed`, the network's
    /// confirmed position (the universe, so `Not` is relative to it), with
    /// the view's balances there (what [`WalletIndex::index`] reads), in
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
    ) -> anyhow::Result<Vec<Index>> {
        read(connection, network, selector, confirmed, true)?
            .into_iter()
            .map(WalletIndex::index)
            .collect()
    }

    /// Of `wanted` (address, asset) on `network`, the fully indexed indexes,
    /// as [`select`] reads them; the rest are left out.
    ///
    /// # Errors
    ///
    /// As [`select`], or when a part of a pair holds a dot.
    pub(crate) fn find(
        connection: &mut SqliteConnection,
        network: &Network,
        wanted: &[(Address, Asset)],
        confirmed: i64,
    ) -> anyhow::Result<Vec<Index>> {
        let ids = wanted
            .iter()
            .map(|(address, asset)| Ok(String::from(WalletIndexId::of(address, asset)?)))
            .collect::<anyhow::Result<_>>()?;
        select(connection, network, &Selector::Ids(ids), confirmed)
    }

    /// The indexes `ids` of `network` (kept ones; fully indexed or not), as
    /// [`select`] reads them, by id.
    ///
    /// # Errors
    ///
    /// As [`select`].
    /// A kept wallet index a pending transaction moves funds of: a row of
    /// [`find_spending`]'s query.
    #[derive(Queryable, Selectable)]
    #[diesel(table_name = wallet_index_with_balance)]
    pub(crate) struct Spending {
        pub(crate) id: String,
        balance_confirmed_spendable: Option<String>,
    }

    impl Spending {
        /// Whether it can spend `takes` (signed: what a send takes is
        /// negative): its `balance_confirmed_spendable` (zero while unknown) plus
        /// `takes` stays at or above zero.
        ///
        /// # Errors
        ///
        /// When a stored amount does not decode.
        pub(crate) fn can_spend(&self, takes: Decimal) -> anyhow::Result<bool> {
            let spendable: Decimal = self
                .balance_confirmed_spendable
                .as_deref()
                .unwrap_or("0")
                .parse()
                .context("a stored balance does not decode")?;
            Ok(spendable + takes >= Decimal::zero())
        }
    }

    /// The first of `ids` not kept, in their order; `None` when each is.
    pub(crate) fn find_missing(
        connection: &mut SqliteConnection,
        ids: &[String],
    ) -> QueryResult<Option<String>> {
        let kept: BTreeSet<String> = wallet_index::table
            .filter(wallet_index::id.eq_any(ids))
            .select(wallet_index::id)
            .load(connection)?
            .into_iter()
            .collect();
        Ok(ids.iter().find(|id| !kept.contains(*id)).cloned())
    }

    /// The kept wallet indexes of `indexes` as a pending transaction spends
    /// from them.
    pub(crate) fn find_spending(
        connection: &mut SqliteConnection,
        indexes: Vec<&String>,
    ) -> QueryResult<Vec<Spending>> {
        wallet_index_with_balance::table
            .filter(wallet_index_with_balance::id.eq_any(indexes))
            .select(Spending::as_select())
            .load(connection)
    }

    #[cfg(test)]
    pub(crate) fn find_by_ids(
        connection: &mut SqliteConnection,
        network: &Network,
        ids: &[impl AsRef<str>],
        confirmed: i64,
    ) -> anyhow::Result<std::collections::BTreeMap<String, WalletIndex>> {
        let picked = Selector::Ids(ids.iter().map(|id| id.as_ref().to_owned()).collect());
        let found = read(connection, network, &picked, confirmed, false)?;
        Ok(found
            .into_iter()
            .map(|one| (one.id.clone().into(), one))
            .collect())
    }

    /// The indexes of `network` that `selector` picks, only those indexing
    /// and indexed through a position at or above `confirmed` when
    /// `fully_indexed`,
    /// with the view's balances at the confirmed position, in one query.
    fn read(
        connection: &mut SqliteConnection,
        network: &Network,
        selector: &Selector,
        confirmed: i64,
        fully_indexed: bool,
    ) -> anyhow::Result<Vec<WalletIndex>> {
        let mut binds = Vec::new();
        let picked = picked(selector, &mut binds);
        let indexed = if fully_indexed {
            "wi.indexed_through >= ? AND wi.state = 0"
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
        rows.into_iter()
            .map(|row| {
                let index = WalletIndex::try_from(row)?;
                let negative = index
                    .balance_confirmed
                    .as_deref()
                    .is_some_and(|total| total.starts_with('-'));
                ensure!(!negative, "a confirmed balance is negative");
                Ok(index)
            })
            .collect()
    }

    /// `selector` as one SQL condition over `wi` (`wallet_index_with_balance`),
    /// its values pushed to `binds` in placeholder order: `All` is AND
    /// (empty: true), `Any` OR (empty: false), `Not` NOT.
    fn picked(selector: &Selector, binds: &mut Vec<String>) -> String {
        let group = |many: &[Selector], binds: &mut Vec<String>, join: &str, empty: &str| {
            if many.is_empty() {
                return empty.to_owned();
            }
            let parts: Vec<String> = many.iter().map(|one| picked(one, binds)).collect();
            format!("({})", parts.join(join))
        };
        match selector {
            Selector::Ids(ids) => {
                binds.extend(ids.iter().cloned());
                format!("wi.id IN ({})", vec!["?"; ids.len()].join(", "))
            }
            Selector::Asset(asset) => {
                binds.push(asset.clone());
                "wi.asset = ?".to_owned()
            }
            Selector::Tag(name) => {
                binds.push(name.clone());
                "EXISTS (SELECT 1 FROM json_each(wi.tags) j WHERE j.value = ?)".to_owned()
            }
            Selector::Untagged {} => "json_array_length(wi.tags) = 0".to_owned(),
            Selector::All(all) => group(all, binds, " AND ", "1"),
            Selector::Any(any) => group(any, binds, " OR ", "0"),
            Selector::Not(inner) => format!("NOT ({})", picked(inner, binds)),
        }
    }
}
