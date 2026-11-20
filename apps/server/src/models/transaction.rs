//! `transaction`: kept transactions, once per txid, pending (no block) or
//! mined in a kept block.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context as _;
use base::{Chain, Decimal, Network, Networked, Reason, TransactionState};
use diesel::{
    deserialize::FromSqlRow,
    expression::AsExpression,
    prelude::*,
    sql_types::{BigInt, Nullable, Text},
    sqlite::Sqlite,
};

use super::{
    Id, IdError, Json, Leg,
    block::BlockId,
    id::typed_id,
    int, leg,
    schema::{self, transaction_with_legs, transactions},
    uint, wallet_index,
};

/// A transaction's id: `{chain}.{network}.{txid}`.
#[derive(Clone, Debug, PartialEq, Eq, AsExpression, FromSqlRow)]
#[diesel(sql_type = Text)]
pub(crate) struct TransactionId {
    pub(crate) network: Network,
    pub(crate) txid: String,
    id: Id,
}

impl TransactionId {
    /// The id of `txid` of `network`.
    ///
    /// # Errors
    ///
    /// When a part holds a dot.
    pub(crate) fn new(network: &Network, txid: &str) -> Result<Self, IdError> {
        let id = Id::new(&[network.chain.as_str(), &network.name, txid])?;
        Ok(Self {
            network: network.clone(),
            txid: txid.to_owned(),
            id,
        })
    }

    /// `text` as a transaction id.
    ///
    /// # Errors
    ///
    /// Unless it has its three parts.
    pub(crate) fn parse(text: &str) -> Result<Self, IdError> {
        let [chain, network, txid] = Id::parts::<3>(text, "{chain}.{network}.{txid}")?;
        Self::new(&Network::new(Chain::new(chain), network), txid)
    }
}

typed_id!(TransactionId, network);

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

/// A stored revert reason as the API shows it: a JSON string decoded
/// (`"reverted"`, not `"\"reverted\""`), anything else as stored.
fn shown_reason<S: serde::Serializer>(
    reason: &Option<String>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let shown = reason
        .as_deref()
        .map(|stored| serde_json::from_str::<String>(stored).unwrap_or_else(|_| stored.to_owned()));
    serde::Serialize::serialize(&shown, serializer)
}

/// A kept transaction: its `transaction` row and its kept legs. The API
/// shows it as is. Read in one query: its legs come as a JSON array SQLite
/// builds (`legs`).
#[derive(
    Clone,
    Debug,
    Insertable,
    AsChangeset,
    Queryable,
    QueryableByName,
    serde::Serialize,
    utoipa::ToSchema,
)]
#[diesel(
    table_name = transactions,
    treat_none_as_null = true,
    check_for_backend(Sqlite)
)]
pub(crate) struct Transaction {
    /// `{chain}.{network}.{txid}`.
    #[schema(value_type = String)]
    pub(crate) id: TransactionId,
    /// Its block; null while pending.
    #[schema(value_type = Option<String>)]
    pub(crate) block: Option<BlockId>,
    pub(crate) chain: String,
    pub(crate) network: String,
    /// Chain-native transaction id.
    pub(crate) txid: String,
    /// Its position in its block; null while pending.
    pub(crate) order: Option<i64>,
    /// 0 pending, 1 succeeded, 2 reverted.
    pub(crate) state: i32,
    /// Why it reverted, when the chain said (`reverted`); null otherwise.
    /// Stored as JSON, shown decoded.
    #[serde(serialize_with = "shown_reason")]
    pub(crate) reason: Option<String>,
    /// The signed transaction in its chain's encoding.
    #[serde(skip)]
    pub(crate) bytes: Vec<u8>,
    /// Unix ms this server recorded it, before its first broadcast: set
    /// exactly on this server's sends, mined or not; null on every other.
    /// Mining keeps it.
    #[diesel(skip_update)]
    pub(crate) broadcasted_at: Option<i64>,
    /// Its block's position; null while pending.
    pub(crate) position: Option<i64>,
    /// Its kept legs, in order.
    #[diesel(
        skip_insertion,
        skip_update,
        sql_type = Text,
        deserialize_as = Json<Vec<Leg>>
    )]
    pub(crate) legs: Vec<Leg>,
}

/// `"{position}:{order}:{broadcasted_at}:{txid}"`, a null as `0` (position
/// `i64::MAX`): one key every list reads the parts it sorts by from.
impl crate::db::Keyed for Transaction {
    fn key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.position.unwrap_or(i64::MAX),
            self.order.unwrap_or(0),
            self.broadcasted_at.unwrap_or(0),
            self.txid
        )
    }
}

/// A page key: (block position, `i64::MAX` while pending; `order`;
/// `broadcasted_at`; `txid`), nulls as `0`.
pub(crate) type Key = (i64, i64, i64, String);

/// The key cursor `text` names, as [`Keyed`](crate::db::Keyed) wrote it.
pub(crate) fn parse_key(text: &str) -> Option<Key> {
    let mut parts = text.splitn(4, ':');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.to_owned(),
    ))
}

impl Networked for Transaction {
    fn network(&self) -> &Network {
        self.id.network()
    }
}

impl Transaction {
    /// Drops its legs of wallet indexes not in `ours` (index ids).
    pub(crate) fn keep(&mut self, ours: &BTreeSet<String>) {
        self.legs
            .retain(|leg| ours.contains(leg.wallet_index_id.as_ref()));
    }

    /// The transaction it was written from, with its kept legs; its
    /// `expires_height` is its transaction index's, not kept here.
    ///
    /// # Errors
    ///
    /// When a stored value does not decode.
    pub(crate) fn transaction(self) -> anyhow::Result<base::Transaction> {
        Ok(base::Transaction {
            id: base::TransactionId::new(self.id.network, self.txid),
            state: decode_state(self.state, self.reason.as_deref())?,
            order: self.order.map(uint).transpose()?,
            legs: self
                .legs
                .into_iter()
                .map(Leg::leg)
                .collect::<anyhow::Result<Vec<_>>>()?,
            bytes: self.bytes,
            expires_height: None,
        })
    }
}

/// `transaction` as kept pending, with every leg (no block).
impl TryFrom<&base::Transaction> for Transaction {
    type Error = anyhow::Error;

    fn try_from(transaction: &base::Transaction) -> anyhow::Result<Self> {
        let network = &transaction.id.network;
        let id = TransactionId::new(network, &transaction.id.id)?;
        let legs = (0_u32..)
            .zip(&transaction.legs)
            .map(|(index, leg)| Leg::try_from((&id, index, leg)))
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
            broadcasted_at: None,
            position: None,
            legs,
        })
    }
}

/// Why a pending transaction is not kept: a wallet index it takes from
/// cannot back what it takes.
#[derive(Debug)]
pub(crate) enum Refused {
    /// It cannot spend what it takes, beyond what is in flight.
    Short,
    /// Index `.0` is not kept: nothing to spend.
    NotIndexed(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Short => formatter.write_str("insufficient-funds"),
            Self::NotIndexed(id) => write!(formatter, "not-indexed: the sender has no index {id}"),
        }
    }
}

impl std::error::Error for Refused {}

/// The `transaction` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;
    use crate::db::Page;

    /// Keeps pending `transaction`, this server's send, with its legs of kept wallet indexes (the others are dropped
    /// from it); a no-op when its txid is kept, pending or mined. Its
    /// `broadcasted_at` is `at` (Unix ms) unless it has one (a version kept
    /// again). Its outgoing legs count as spent until it is mined or
    /// forgotten.
    ///
    /// # Errors
    ///
    /// [`Refused`] when a wallet index it takes from (every leg counts,
    /// kept or not) is not kept, or cannot spend what it takes beyond what
    /// is in flight (the caller's write must roll back: the row is already
    /// inserted); when a value does not decode or the write fails.
    pub(crate) fn save_pending(
        connection: &mut SqliteConnection,
        transaction: &mut Transaction,
        at: i64,
    ) -> anyhow::Result<()> {
        transaction.broadcasted_at.get_or_insert(at);
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
                .entry(leg.wallet_index_id.clone().into())
                .or_insert_with(Decimal::zero) += leg.signed()?;
        }
        let ours: BTreeMap<String, wallet_index::db::Spending> =
            wallet_index::db::find_spending(connection, takes.keys().collect())?
                .into_iter()
                .map(|index| (index.id.clone(), index))
                .collect();
        for (id, takes) in &takes {
            if *takes >= Decimal::zero() {
                continue;
            }
            if !sender(&ours, id)?.can_spend(takes.clone())? {
                anyhow::bail!(Refused::Short);
            }
        }
        // A counterparty this server does not index leaves no leg.
        let ours = ours.into_keys().collect();
        transaction.keep(&ours);
        leg::db::insert(connection, transaction.legs.iter().collect())?;
        Ok(())
    }

    /// Index `id` of `kept`, which a send takes from.
    ///
    /// # Errors
    ///
    /// [`Refused`] unless it is kept. Paused or without data, it may
    /// still send: the key, not the index, spends.
    fn sender<'a>(
        kept: &'a BTreeMap<String, wallet_index::db::Spending>,
        id: &str,
    ) -> anyhow::Result<&'a wallet_index::db::Spending> {
        Ok(kept
            .get(id)
            .ok_or_else(|| Refused::NotIndexed(id.to_owned()))?)
    }

    /// Marks transaction `id`, kept pending, mined at `position` with its
    /// outcome (state succeeded, or reverted without a reason), as the
    /// chain's `Confirmer` judged it: no block is kept for it. A reorg's
    /// [`set_pending_from`] sets it pending again.
    ///
    /// # Errors
    ///
    /// When a value is out of range or the write fails.
    pub(crate) fn set_mined(
        connection: &mut SqliteConnection,
        id: impl AsRef<str>,
        position: u64,
        reverted: bool,
    ) -> anyhow::Result<()> {
        diesel::update(
            transactions::table
                .find(id.as_ref())
                .filter(transactions::block.is_null()),
        )
        .set((
            transactions::position.eq(int(position)?),
            transactions::state.eq(if reverted { 2 } else { 1 }),
        ))
        .execute(connection)?;
        Ok(())
    }

    /// Keeps mined `transaction`'s row: a pending or earlier mined row
    /// takes its values, but `broadcasted_at` (this server's send stays
    /// one).
    ///
    /// # Errors
    ///
    /// When a write fails.
    pub(crate) fn upsert(
        connection: &mut SqliteConnection,
        transaction: &Transaction,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            transaction.block.is_some() && transaction.position.is_some(),
            "a mined transaction has no block"
        );
        diesel::insert_into(transactions::table)
            .values(transaction)
            .on_conflict(transactions::id)
            .do_update()
            .set(transaction)
            .execute(connection)?;
        Ok(())
    }

    /// `network`'s transactions mined at or above `from`.
    macro_rules! mined_from {
        ($network:expr, $from:expr) => {
            transactions::table
                .filter(transactions::chain.eq($network.chain.as_str()))
                .filter(transactions::network.eq(&$network.name))
                .filter(transactions::position.ge($from))
        };
    }

    /// Deletes `network`'s transactions mined at or above `from` that this
    /// server did not broadcast (`broadcasted_at` NULL): a reorg's, their
    /// legs with them (the `leg.transaction` cascade); they come back when
    /// the new fork is read. How many.
    pub(crate) fn delete_unbroadcast_from(
        connection: &mut SqliteConnection,
        network: &Network,
        from: i64,
    ) -> QueryResult<usize> {
        diesel::delete(mined_from!(network, from).filter(transactions::broadcasted_at.is_null()))
            .execute(connection)
    }

    /// Sets `network`'s transactions mined at or above `from` pending (no
    /// block, position or order, state pending): a reorg's, once only this
    /// server's sends are left there, so their spend stays counted while
    /// they may land in the new fork. How many.
    pub(crate) fn set_pending_from(
        connection: &mut SqliteConnection,
        network: &Network,
        from: i64,
    ) -> QueryResult<usize> {
        diesel::update(mined_from!(network, from))
            .set((
                transactions::block.eq(None::<String>),
                transactions::position.eq(None::<i64>),
                transactions::order.eq(None::<i64>),
                transactions::state.eq(0),
                transactions::reason.eq(None::<String>),
            ))
            .execute(connection)
    }

    /// Transaction `id` with its kept legs, if kept.
    pub(crate) fn get(
        connection: &mut SqliteConnection,
        id: impl AsRef<str>,
    ) -> QueryResult<Option<Transaction>> {
        transaction_with_legs::table
            .find(id.as_ref())
            .get_result(connection)
            .optional()
    }

    /// Of every network: how many transactions are kept mined, how many
    /// of those reverted, and how many are pending.
    pub(crate) fn count(connection: &mut SqliteConnection) -> QueryResult<(i64, i64, i64)> {
        transactions::table
            .select(diesel::dsl::sql::<(BigInt, BigInt, BigInt)>(
                r#"
                COUNT(block),
                COALESCE(SUM(block IS NOT NULL AND state = 2), 0),
                COALESCE(SUM(block IS NULL), 0)"#,
            ))
            .get_result(connection)
    }

    /// How many kept transactions of every network are mined in blocks
    /// stamped in each hour from `since` (Unix seconds) on, as
    /// (hour, count); hour 0 starts at `since`.
    pub(crate) fn count_by_hour(
        connection: &mut SqliteConnection,
        since: i64,
    ) -> QueryResult<Vec<(i64, i64)>> {
        use crate::models::schema::block;
        transactions::table
            .inner_join(block::table.on(block::id.nullable().eq(transactions::block)))
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

    /// At most `page.limit` transactions of `network`, pending or mined,
    /// with their legs, newest first: pending by `broadcasted_at`,
    /// then mined by (position, `order`); after `page.after` when given.
    pub(crate) fn find_page(
        connection: &mut SqliteConnection,
        network: &Network,
        page: Page<Key>,
    ) -> QueryResult<Vec<Transaction>> {
        let (position, order, broadcasted_at, txid) = page.after.map_or(
            (0, 0, 0, None),
            |(position, order, broadcasted_at, txid)| (position, order, broadcasted_at, Some(txid)),
        );
        diesel::sql_query(
            r#"
            SELECT
                t.*
            FROM transaction_with_legs t
            WHERE t.chain = ?1
              AND t.network = ?2
              AND (?6 IS NULL OR (
                  COALESCE(t.position, 9223372036854775807),
                  COALESCE(t."order", 0),
                  COALESCE(t.broadcasted_at, 0),
                  t.txid
              ) < (?3, ?4, ?5, ?6))
            ORDER BY
                COALESCE(t.position, 9223372036854775807) DESC,
                COALESCE(t."order", 0) DESC,
                COALESCE(t.broadcasted_at, 0) DESC,
                t.txid DESC
            LIMIT ?7"#,
        )
        .bind::<Text, _>(network.chain.as_str())
        .bind::<Text, _>(&network.name)
        .bind::<BigInt, _>(position)
        .bind::<BigInt, _>(order)
        .bind::<BigInt, _>(broadcasted_at)
        .bind::<Nullable<Text>, _>(txid)
        .bind::<BigInt, _>(page.limit)
        .load(connection)
    }

    /// At most `page.limit` pending transactions of `network` (its mempool),
    /// with their legs, oldest broadcast first; after `page.after` when
    /// given.
    pub(crate) fn find_pending_page(
        connection: &mut SqliteConnection,
        network: &Network,
        page: Page<Key>,
    ) -> QueryResult<Vec<Transaction>> {
        let (broadcasted_at, txid) = page
            .after
            .map_or((0, None), |(_, _, broadcasted_at, txid)| {
                (broadcasted_at, Some(txid))
            });
        diesel::sql_query(
            r#"
            SELECT
                t.*
            FROM transaction_with_legs t
            WHERE t.chain = ?1
              AND t.network = ?2
              AND t.block IS NULL
              AND (?4 IS NULL OR (COALESCE(t.broadcasted_at, 0), t.txid) > (?3, ?4))
            ORDER BY
                COALESCE(t.broadcasted_at, 0),
                t.txid
            LIMIT ?5"#,
        )
        .bind::<Text, _>(network.chain.as_str())
        .bind::<Text, _>(&network.name)
        .bind::<BigInt, _>(broadcasted_at)
        .bind::<Nullable<Text>, _>(txid)
        .bind::<BigInt, _>(page.limit)
        .load(connection)
    }

    /// At most `page.limit` transactions of block `block` (an id), with
    /// their legs, in block order; after `page.after` when given.
    pub(crate) fn find_by_block_page(
        connection: &mut SqliteConnection,
        block: impl AsRef<str>,
        page: Page<Key>,
    ) -> QueryResult<Vec<Transaction>> {
        let (order, txid) = page
            .after
            .map_or((0, None), |(_, order, _, txid)| (order, Some(txid)));
        diesel::sql_query(
            r#"
            SELECT
                t.*
            FROM transaction_with_legs t
            WHERE t.block = ?1
              AND (?3 IS NULL OR (COALESCE(t."order", 0), t.txid) > (?2, ?3))
            ORDER BY
                COALESCE(t."order", 0),
                t.txid
            LIMIT ?4"#,
        )
        .bind::<Text, _>(block.as_ref())
        .bind::<BigInt, _>(order)
        .bind::<Nullable<Text>, _>(txid)
        .bind::<BigInt, _>(page.limit)
        .load(connection)
    }

    /// The pending transactions `sender` pays from (a leg of it spent,
    /// burned or a fee), with their legs.
    pub(crate) fn find_sent_by(
        connection: &mut SqliteConnection,
        sender: &base::Address,
    ) -> QueryResult<Vec<Transaction>> {
        diesel::sql_query(
            r#"
            SELECT
                t.*
            FROM transaction_with_legs t
            WHERE t.chain = ?1
              AND t.network = ?2
              AND t.block IS NULL
              AND EXISTS (
                  SELECT 1
                  FROM leg l
                  WHERE l."transaction" = t.id
                    AND l.address = ?3
                    AND l.kind IN (1, 2, 4)
              )"#,
        )
        .bind::<Text, _>(sender.network.chain.as_str())
        .bind::<Text, _>(&sender.network.name)
        .bind::<Text, _>(&sender.value)
        .load(connection)
    }

    /// Deletes the pending transactions `ids`, their legs with them (the
    /// schema's cascade): their funds and coins are free again. A mined one
    /// is kept. The ids it deleted.
    pub(crate) fn delete_pending(
        connection: &mut SqliteConnection,
        ids: &[impl AsRef<str>],
    ) -> QueryResult<Vec<String>> {
        let ids: Vec<&str> = ids.iter().map(AsRef::as_ref).collect();
        diesel::delete(
            transactions::table
                .filter(transactions::id.eq_any(ids))
                .filter(transactions::block.is_null()),
        )
        .returning(transactions::id)
        .load(connection)
    }

    /// The ids of the pending transactions wallet index `index` sends (a
    /// leg of it spent, burned or a fee): what a jump forgets, as one mined
    /// in its gap is never read.
    pub(crate) fn find_sent(
        connection: &mut SqliteConnection,
        index: impl AsRef<str>,
    ) -> QueryResult<Vec<String>> {
        schema::leg::table
            .filter(schema::leg::wallet_index_id.eq(index.as_ref()))
            .filter(schema::leg::block.is_null())
            .filter(schema::leg::kind.eq_any([1, 2, 4]))
            .select(schema::leg::transaction.assume_not_null())
            .distinct()
            .load(connection)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json, value::Serializer};

    use super::shown_reason;

    #[test]
    fn a_stored_reason_is_shown_decoded() {
        let shown = |stored: Option<&str>| -> Value {
            shown_reason(&stored.map(str::to_owned), Serializer).expect("a value")
        };
        assert_eq!(shown(Some(r#""reverted""#)), json!("reverted"));
        assert_eq!(shown(Some(r#"{"other":"x"}"#)), json!(r#"{"other":"x"}"#));
        assert_eq!(shown(None), Value::Null);
    }
}
