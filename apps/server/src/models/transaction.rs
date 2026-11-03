//! `transaction`: kept transactions, once per txid, pending (no block) or
//! mined in a kept block.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context as _;
use base::{Address, Asset, Decimal, Network, Reason, TransactionId, TransactionState};
use diesel::{
    prelude::*,
    sql_types::{BigInt, Bool, Text},
    sqlite::Sqlite,
};

use super::{
    Json, Leg, int, key, leg,
    schema::{self, transactions},
    uint,
};

/// The `state` and `reason` columns of `state`.
///
/// # Errors
///
/// When a revert reason does not encode.
pub(crate) fn encode_state(state: &TransactionState) -> anyhow::Result<(i32, Option<String>)> {
    Ok(match state {
        TransactionState::Pending => (0, None),
        TransactionState::Succeeded => (1, None),
        TransactionState::Reverted(reason) => (
            2,
            reason
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .context("a revert reason did not encode")?,
        ),
    })
}

/// The state stored as `state` and `reason`.
///
/// # Errors
///
/// When a stored revert reason does not decode.
pub(crate) fn decode_state(state: i32, reason: Option<&str>) -> anyhow::Result<TransactionState> {
    Ok(match state {
        0 => TransactionState::Pending,
        1 => TransactionState::Succeeded,
        _ => TransactionState::Reverted(
            reason
                .map(serde_json::from_str::<Reason>)
                .transpose()
                .context("a stored revert reason does not decode")?,
        ),
    })
}

/// A kept transaction: its `transaction` row and its kept legs. The API
/// shows it as is. Read in one query: its legs come as a JSON array SQLite
/// builds (`legs`).
#[derive(
    Clone,
    Debug,
    Insertable,
    AsChangeset,
    QueryableByName,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[diesel(
    table_name = transactions,
    treat_none_as_null = true,
    check_for_backend(Sqlite)
)]
pub(crate) struct Transaction {
    /// `{chain}.{network}.{txid}`.
    pub(crate) id: String,
    /// Its block; null while pending.
    pub(crate) block: Option<String>,
    pub(crate) chain: String,
    pub(crate) network: String,
    /// Chain-native transaction id.
    pub(crate) txid: String,
    /// Its position in its block; null while pending.
    pub(crate) order: Option<i64>,
    /// 0 pending, 1 succeeded, 2 reverted.
    pub(crate) state: i32,
    /// Why it reverted (JSON), when the chain said; null otherwise.
    pub(crate) reason: Option<String>,
    /// The signed transaction in its chain's encoding; empty when read
    /// nested in a block.
    #[serde(skip)]
    pub(crate) bytes: Vec<u8>,
    /// Unix ms it was recorded as pending; null once mined.
    pub(crate) sent_at: Option<i64>,
    /// Unix ms it stops holding funds; null once mined.
    pub(crate) expires_at: Option<i64>,
    /// Its kept legs, in order.
    #[diesel(
        skip_insertion,
        skip_update,
        sql_type = Text,
        deserialize_as = Json<Vec<Leg>>
    )]
    pub(crate) legs: Vec<Leg>,
}

/// Transactions parsed from a JSON column (an array SQLite built).
impl From<Json<Vec<Transaction>>> for Vec<Transaction> {
    fn from(Json(transactions): Json<Vec<Transaction>>) -> Self {
        transactions
    }
}

impl Transaction {
    /// The id of `txid` of `network`: `"{chain}.{network}.{txid}"`.
    ///
    /// # Errors
    ///
    /// When a part holds a dot.
    pub(crate) fn id(network: &Network, txid: &str) -> anyhow::Result<String> {
        key(&[network.chain.as_str(), &network.name, txid])
    }

    /// Drops its legs of wallet indexes not in `ours` (index ids).
    pub(crate) fn keep(&mut self, ours: &BTreeSet<String>) {
        self.legs.retain(|leg| ours.contains(&leg.wallet_index_id));
    }

    /// The transaction it was written from, with its kept legs.
    ///
    /// # Errors
    ///
    /// When a stored value does not decode.
    pub(crate) fn transaction(self) -> anyhow::Result<base::Transaction> {
        let network = Network::new(base::Chain::new(&self.chain), &self.network);
        Ok(base::Transaction {
            id: TransactionId::new(network.clone(), self.txid),
            state: decode_state(self.state, self.reason.as_deref())?,
            order: self.order.map(uint).transpose()?,
            legs: self
                .legs
                .into_iter()
                .map(|leg| leg.leg(&network))
                .collect::<anyhow::Result<Vec<_>>>()?,
            bytes: self.bytes,
            expires: self.expires_at.map(uint).transpose()?,
        })
    }
}

/// `transaction` as kept pending, with every leg (no block).
impl TryFrom<&base::Transaction> for Transaction {
    type Error = anyhow::Error;

    fn try_from(transaction: &base::Transaction) -> anyhow::Result<Self> {
        let network = &transaction.id.network;
        let id = Self::id(network, &transaction.id.id)?;
        let legs = (0_u32..)
            .zip(&transaction.legs)
            .map(|(index, leg)| Leg::try_from((id.as_str(), index, leg)))
            .collect::<anyhow::Result<_>>()?;
        let (state, reason) = encode_state(&transaction.state)?;
        Ok(Self {
            id,
            block: None,
            chain: network.chain.to_string(),
            network: network.name.clone(),
            txid: transaction.id.id.clone(),
            order: transaction.order.map(int).transpose()?,
            state,
            reason,
            bytes: transaction.bytes.clone(),
            sent_at: None,
            expires_at: transaction.expires.map(int).transpose()?,
            legs,
        })
    }
}

/// An address cannot spend what a pending transaction takes, beyond what is
/// in flight.
#[derive(Debug)]
pub(crate) struct Short;

impl std::fmt::Display for Short {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("insufficient-funds")
    }
}

impl std::error::Error for Short {}

/// The `transaction` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    /// At most `limit` transactions of `address` in `asset`, in live blocks,
    /// after the (`position`, `order`, `txid`) key, each with only its legs
    /// of `address` in `asset`.
    pub(crate) fn find_page(
        connection: &mut SqliteConnection,
        address: &Address,
        asset: &Asset,
        after: (i64, i64, &str),
        limit: i64,
    ) -> QueryResult<Vec<Transaction>> {
        // One page of the transactions in live blocks with a leg of an address
        // in an asset, after a (position, order, txid) key, in that order, each
        // with only those legs.
        diesel::sql_query(
            r#"
            SELECT
                t.*,
                COALESCE((
                    SELECT json_group_array(
                        json(l.json)
                        ORDER BY l.leg_index
                    )
                    FROM leg_json l
                    WHERE l."transaction" = t.id
                      AND l.address = ?3
                      AND l.asset = ?4
                ), '[]') AS legs
            FROM "transaction" t
            JOIN block b ON b.id = t.block
            WHERE b.chain = ?1
              AND b.network = ?2
              AND b.archived_at IS NULL
              AND EXISTS (
                  SELECT 1
                  FROM leg l
                  WHERE l."transaction" = t.id
                    AND l.address = ?3
                    AND l.asset = ?4
              )
              AND (b.position, COALESCE(t."order", 0), t.txid) > (?5, ?6, ?7)
            ORDER BY b.position, COALESCE(t."order", 0), t.txid
            LIMIT ?8"#,
        )
        .bind::<Text, _>(address.network.chain.as_str())
        .bind::<Text, _>(&address.network.name)
        .bind::<Text, _>(&address.value)
        .bind::<Text, _>(&asset.id)
        .bind::<BigInt, _>(after.0)
        .bind::<BigInt, _>(after.1)
        .bind::<Text, _>(after.2)
        .bind::<BigInt, _>(limit)
        .load(connection)
    }

    /// Keeps pending `transaction` with its legs of kept wallet indexes
    /// (the others are dropped from it); a no-op when its txid is kept,
    /// pending or mined. Its outgoing legs count as spent until it is mined,
    /// deleted or expired.
    ///
    /// # Errors
    ///
    /// [`Short`] when a wallet index it takes from (every leg counts, kept
    /// or not) cannot spend what it takes, beyond what is in flight (the
    /// caller's write must roll back: the row is already inserted); when a
    /// value does not decode or the write fails.
    pub(crate) fn save_pending(
        connection: &mut SqliteConnection,
        transaction: &mut Transaction,
    ) -> anyhow::Result<()> {
        let added = diesel::insert_into(transactions::table)
            .values(&*transaction)
            .on_conflict_do_nothing()
            .execute(connection)?;
        if added == 0 {
            return Ok(());
        }
        // Checked before its own legs count, in one read over its indexes;
        // failing rolls the caller's write back.
        let mut takes = BTreeMap::<String, Decimal>::new();
        for leg in &transaction.legs {
            *takes
                .entry(leg.wallet_index_id.clone())
                .or_insert_with(Decimal::zero) += leg.signed()?;
        }
        let indexes = leg::db::find_spending(connection, takes.keys().collect())?;
        for index in &indexes {
            let takes = takes.get(&index.id).context("an index was not read")?;
            if *takes < Decimal::zero() && !index.can_spend(takes.clone())? {
                anyhow::bail!(Short);
            }
        }
        // A counterparty this server does not index leaves no leg.
        let ours = indexes
            .into_iter()
            .filter(|index| index.ours)
            .map(|index| index.id)
            .collect();
        transaction.keep(&ours);
        leg::db::insert(connection, transaction.legs.iter().collect())?;
        Ok(())
    }

    /// Keeps mined `transaction`'s row: a pending or earlier mined row
    /// takes its values, and its legs of no block or another block (pending,
    /// or mined before a reorg) are deleted. Its own legs are the caller's.
    ///
    /// # Errors
    ///
    /// When a write fails.
    pub(crate) fn upsert(
        connection: &mut SqliteConnection,
        transaction: &Transaction,
    ) -> anyhow::Result<()> {
        let block = transaction
            .block
            .as_deref()
            .context("a mined transaction has no block")?;
        diesel::insert_into(transactions::table)
            .values(transaction)
            .on_conflict(transactions::id)
            .do_update()
            .set(transaction)
            .execute(connection)?;
        leg::db::delete_moved(connection, block, &transaction.id)?;
        Ok(())
    }

    /// Transaction `id` with its kept legs, if kept ([`ONE`]).
    pub(crate) fn get(
        connection: &mut SqliteConnection,
        id: &str,
    ) -> QueryResult<Option<Transaction>> {
        // Transaction `?1` with its legs.
        diesel::sql_query(
            r#"
            SELECT
                t.*,
                COALESCE((
                    SELECT json_group_array(
                        json(l.json)
                        ORDER BY l.leg_index
                    )
                    FROM leg_json l
                    WHERE l."transaction" = t.id
                ), '[]') AS legs
            FROM "transaction" t
            WHERE t.id = ?1"#,
        )
        .bind::<Text, _>(id)
        .get_result(connection)
        .optional()
    }

    /// Of every network: how many transactions are kept mined in live
    /// blocks, how many of those reverted, and how many are pending.
    pub(crate) fn count(connection: &mut SqliteConnection) -> QueryResult<(i64, i64, i64)> {
        use crate::models::schema::block;
        // A mined transaction joins its block only while that is live.
        transactions::table
            .left_join(
                block::table.on(block::id
                    .nullable()
                    .eq(transactions::block)
                    .and(block::archived_at.is_null())),
            )
            .select(diesel::dsl::sql::<(BigInt, BigInt, BigInt)>(
                r#"
                COUNT(block.id),
                COALESCE(SUM(block.id IS NOT NULL AND "transaction".state = 2), 0),
                COALESCE(SUM("transaction".block IS NULL), 0)"#,
            ))
            .get_result(connection)
    }

    /// How many kept transactions of every network are mined in live
    /// blocks stamped in each hour from `since` (Unix seconds) on, as
    /// (hour, count); hour 0 starts at `since`.
    pub(crate) fn count_by_hour(
        connection: &mut SqliteConnection,
        since: i64,
    ) -> QueryResult<Vec<(i64, i64)>> {
        use crate::models::schema::block;
        transactions::table
            .inner_join(block::table.on(block::id.nullable().eq(transactions::block)))
            .filter(block::archived_at.is_null())
            .filter(block::timestamp.ge(since))
            .group_by(diesel::dsl::sql::<BigInt>("hour"))
            .select((
                diesel::dsl::sql::<BigInt>("(block.timestamp - ")
                    .bind::<BigInt, _>(since)
                    .sql(") / 3600 AS hour"),
                diesel::dsl::count_star(),
            ))
            .load(connection)
    }

    /// At most `limit` pending transactions of `network` with their legs,
    /// oldest sent first.
    pub(crate) fn find_by_network(
        connection: &mut SqliteConnection,
        network: &Network,
        pending: bool,
        limit: i64,
    ) -> QueryResult<Vec<Transaction>> {
        // At most `?4` transactions of a network with their legs: pending ones
        // (`?3` true), oldest sent first, or mined ones in live blocks (`?3`
        // false), latest first.
        diesel::sql_query(
            r#"
            SELECT
                t.*,
                COALESCE((
                    SELECT json_group_array(
                        json(l.json)
                        ORDER BY l.leg_index
                    )
                    FROM leg_json l
                    WHERE l."transaction" = t.id
                ), '[]') AS legs
            FROM "transaction" t
            LEFT JOIN block b ON b.id = t.block
            WHERE t.chain = ?1
              AND t.network = ?2
              AND CASE
                  WHEN ?3 THEN t.block IS NULL
                  ELSE b.archived_at IS NULL AND t.block IS NOT NULL
              END
            ORDER BY
                b.position DESC,
                t."order" DESC,
                t.sent_at
            LIMIT ?4"#,
        )
        .bind::<Text, _>(network.chain.as_str())
        .bind::<Text, _>(&network.name)
        .bind::<Bool, _>(pending)
        .bind::<BigInt, _>(limit)
        .load(connection)
    }

    /// Forgets the pending transactions `ids` and their legs (never sent,
    /// refused): their funds and coins are free again. A mined one is kept.
    pub(crate) fn delete_pending(
        connection: &mut SqliteConnection,
        ids: &[String],
    ) -> QueryResult<usize> {
        let pending = transactions::table
            .filter(transactions::id.eq_any(ids))
            .filter(transactions::block.is_null());
        diesel::delete(schema::leg::table.filter(
            schema::leg::transaction.eq_any(pending.clone().select(transactions::id.nullable())),
        ))
        .execute(connection)?;
        diesel::delete(pending).execute(connection)
    }

    /// Forgets every pending transaction of `network` past its expiry at
    /// `now`, with its legs; how many.
    pub(crate) fn expire(
        connection: &mut SqliteConnection,
        network: &Network,
        now: i64,
    ) -> QueryResult<usize> {
        let expired = transactions::table
            .filter(transactions::chain.eq(network.chain.as_str()))
            .filter(transactions::network.eq(&network.name))
            .filter(transactions::block.is_null())
            .filter(transactions::expires_at.le(now));
        diesel::delete(
            schema::leg::table.filter(
                schema::leg::transaction.eq_any(expired.select(transactions::id.nullable())),
            ),
        )
        .execute(connection)?;
        diesel::delete(expired).execute(connection)
    }
}
