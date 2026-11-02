//! `transaction`: kept transactions, once per txid, pending (no block) or
//! mined in a kept block.

use anyhow::Context as _;
use base::{Address, Asset, Decimal, Network, Reason, TransactionId, TransactionState};
use diesel::{
    prelude::*,
    sql_types::{BigInt, Text},
    sqlite::Sqlite,
};

use super::{Leg, Legs, int, key, leg, schema::transactions, uint};

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

/// One `transaction` row.
#[derive(Clone, Debug, Queryable, Selectable, QueryableByName)]
#[diesel(table_name = transactions, check_for_backend(Sqlite))]
pub(crate) struct Transaction {
    pub(crate) id: String,
    pub(crate) block: Option<String>,
    pub(crate) chain: String,
    pub(crate) network: String,
    pub(crate) txid: String,
    pub(crate) order: Option<i64>,
    pub(crate) state: i32,
    pub(crate) reason: Option<String>,
    pub(crate) bytes: Vec<u8>,
    #[allow(dead_code, reason = "the whole row is selected; kept as a record")]
    pub(crate) sent_at: Option<i64>,
    pub(crate) expires_at: Option<i64>,
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
}

impl Transaction {
    /// The transaction this row was written from, with `legs` (its own, in
    /// order).
    ///
    /// # Errors
    ///
    /// When a stored value does not decode.
    pub(crate) fn transaction(self, legs: Legs) -> anyhow::Result<base::Transaction> {
        let network = Network::new(base::Chain::new(&self.chain), &self.network);
        Ok(base::Transaction {
            id: TransactionId::new(network.clone(), self.txid),
            state: decode_state(self.state, self.reason.as_deref())?,
            order: self.order.map(uint).transpose()?,
            legs: legs
                .into_iter()
                .map(|leg| leg.leg(&network))
                .collect::<anyhow::Result<Vec<_>>>()?,
            bytes: self.bytes,
            expires: self.expires_at.map(uint).transpose()?,
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

collection! {
    /// Transactions, in the order read.
    Transactions(Transaction)
}

/// The `transaction` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    #[derive(Insertable, AsChangeset)]
    #[diesel(table_name = transactions, treat_none_as_null = true)]
    struct NewTransaction<'a> {
        id: &'a str,
        block: Option<&'a str>,
        chain: &'a str,
        network: &'a str,
        txid: &'a str,
        order: Option<i64>,
        state: i32,
        reason: Option<String>,
        bytes: &'a [u8],
        sent_at: Option<i64>,
        expires_at: Option<i64>,
    }

    /// One page of the transactions in live blocks with a leg of an address in
    /// an asset, after a (position, order, txid) key, in that order.
    const PAGE: &str = r#"
    SELECT t.* FROM "transaction" t
    JOIN block b ON b.id = t.block
    WHERE b.chain = ?1 AND b.network = ?2 AND b.archived_at IS NULL
      AND EXISTS (SELECT 1 FROM leg l
        WHERE l."transaction" = t.id AND l.address = ?3 AND l.asset = ?4)
      AND (b.position, COALESCE(t."order", 0), t.txid) > (?5, ?6, ?7)
    ORDER BY b.position, COALESCE(t."order", 0), t.txid
    LIMIT ?8"#;

    /// At most `limit` transactions of `address` in `asset`, in live blocks,
    /// after the (`position`, `order`, `txid`) key (see [`PAGE`]).
    pub(crate) fn find_page(
        connection: &mut SqliteConnection,
        address: &Address,
        asset: &Asset,
        after: (i64, i64, &str),
        limit: i64,
    ) -> QueryResult<Transactions> {
        diesel::sql_query(PAGE)
            .bind::<Text, _>(address.network.chain.as_str())
            .bind::<Text, _>(&address.network.name)
            .bind::<Text, _>(&address.value)
            .bind::<Text, _>(&asset.id)
            .bind::<BigInt, _>(after.0)
            .bind::<BigInt, _>(after.1)
            .bind::<Text, _>(after.2)
            .bind::<BigInt, _>(limit)
            .load::<Transaction>(connection)
            .map(Transactions::from)
    }

    /// Keeps pending `transaction`, recorded at `now`, with every leg (no
    /// block); a no-op when its txid is kept, pending or mined. Its outgoing
    /// legs count as spent until it is mined, deleted or expired.
    ///
    /// # Errors
    ///
    /// [`Short`] when an address it takes from cannot spend what it takes,
    /// beyond what is in flight (the caller's write must roll back: the row
    /// is already inserted); when a value does not encode or the write
    /// fails.
    pub(crate) fn insert_pending(
        connection: &mut SqliteConnection,
        transaction: &base::Transaction,
        now: i64,
    ) -> anyhow::Result<()> {
        let (state, reason) = encode_state(&transaction.state)?;
        let network = &transaction.id.network;
        let id = Transaction::id(network, &transaction.id.id)?;
        let row = NewTransaction {
            id: &id,
            block: None,
            chain: network.chain.as_str(),
            network: &network.name,
            txid: &transaction.id.id,
            order: transaction.order.map(int).transpose()?,
            state,
            reason,
            bytes: &transaction.bytes,
            sent_at: Some(now),
            expires_at: transaction.expires.map(int).transpose()?,
        };
        let added = diesel::insert_or_ignore_into(transactions::table)
            .values(&row)
            .execute(connection)?;
        if added == 0 {
            return Ok(());
        }
        // Checked before its own legs count; failing rolls the caller's
        // write back.
        for ((address, asset), takes) in transaction.debits() {
            let spendable = leg::db::find_spendable(connection, &address, &asset)?;
            if spendable.sum()? + takes < Decimal::zero() {
                anyhow::bail!(Short);
            }
        }
        let legs = (0_u32..)
            .zip(&transaction.legs)
            .map(|(index, leg)| Leg::new(None, &id, index, leg))
            .collect::<anyhow::Result<Legs>>()?;
        leg::db::insert(connection, &legs)?;
        Ok(())
    }

    /// Keeps `transaction` as mined in block `block` with `legs` (indexed
    /// legs of it): the row takes the mined values and its legs of no block
    /// or another block (pending, or mined before a reorg) are replaced.
    ///
    /// # Errors
    ///
    /// When a value does not encode or a write fails.
    pub(crate) fn upsert_mined<'l>(
        connection: &mut SqliteConnection,
        block: &str,
        transaction: &base::Transaction,
        legs: impl IntoIterator<Item = (u32, &'l base::Leg)>,
    ) -> anyhow::Result<()> {
        let (state, reason) = encode_state(&transaction.state)?;
        let network = &transaction.id.network;
        let id = Transaction::id(network, &transaction.id.id)?;
        let row = NewTransaction {
            id: &id,
            block: Some(block),
            chain: network.chain.as_str(),
            network: &network.name,
            txid: &transaction.id.id,
            order: transaction.order.map(int).transpose()?,
            state,
            reason,
            bytes: &transaction.bytes,
            sent_at: None,
            expires_at: None,
        };
        diesel::insert_into(transactions::table)
            .values(&row)
            .on_conflict(transactions::id)
            .do_update()
            .set(&row)
            .execute(connection)?;
        let rows = legs
            .into_iter()
            .map(|(index, leg)| Leg::new(Some(block), &id, index, leg))
            .collect::<anyhow::Result<Legs>>()?;
        leg::db::replace(connection, block, &id, &rows)?;
        Ok(())
    }

    /// The pending transaction `id`, if it is still pending.
    pub(crate) fn find_pending(
        connection: &mut SqliteConnection,
        id: &str,
    ) -> QueryResult<Option<Transaction>> {
        transactions::table
            .filter(transactions::id.eq(id))
            .filter(transactions::block.is_null())
            .select(Transaction::as_select())
            .first(connection)
            .optional()
    }

    /// Forgets pending transaction `id` and its legs (refused, expired):
    /// its funds and coins are free again. A mined one is kept.
    pub(crate) fn delete_by_id(connection: &mut SqliteConnection, id: &str) -> QueryResult<usize> {
        leg::db::delete_for(connection, id)?;
        diesel::delete(
            transactions::table
                .filter(transactions::id.eq(id))
                .filter(transactions::block.is_null()),
        )
        .execute(connection)
    }

    /// Forgets every pending transaction of `network` past its expiry at
    /// `now`, with its legs; how many.
    pub(crate) fn expire(
        connection: &mut SqliteConnection,
        network: &Network,
        now: i64,
    ) -> QueryResult<usize> {
        let ids: Vec<String> = transactions::table
            .filter(transactions::chain.eq(network.chain.as_str()))
            .filter(transactions::network.eq(&network.name))
            .filter(transactions::block.is_null())
            .filter(transactions::expires_at.le(now))
            .select(transactions::id)
            .load(connection)?;
        for id in &ids {
            delete_by_id(connection, id)?;
        }
        Ok(ids.len())
    }
}
