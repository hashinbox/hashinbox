//! `transaction`: kept transactions, once per txid, pending (no block) or
//! mined in a kept block.

use anyhow::Context as _;
use base::{Address, Asset, Leg, Network, Reason, Transaction, TransactionId, TransactionState};
use diesel::{
    prelude::*,
    sql_types::{BigInt, Text},
    sqlite::Sqlite,
};

use super::{LegFacts, LegRow, int, key, schema::transactions, uint};

/// One `transaction` row.
#[derive(Clone, Debug, Queryable, Selectable, QueryableByName)]
#[diesel(table_name = transactions, check_for_backend(Sqlite))]
pub(crate) struct TransactionRow {
    pub(crate) id: String,
    pub(crate) block: Option<String>,
    #[allow(
        dead_code,
        reason = "the whole row is selected; the network is the query's"
    )]
    pub(crate) chain: String,
    #[allow(
        dead_code,
        reason = "the whole row is selected; the network is the query's"
    )]
    pub(crate) network: String,
    pub(crate) txid: String,
    pub(crate) order: Option<i64>,
    pub(crate) state: i32,
    pub(crate) reason: Option<String>,
    pub(crate) bytes: Vec<u8>,
}

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
}

/// Every transaction pending or in a live block with a leg of an address:
/// mined ones in chain order, then pending ones. One whose block is archived
/// is not shown (a reorg cannot make funds appear).
const FOR_ADDRESS: &str = r#"
SELECT t.* FROM "transaction" t
LEFT JOIN block b ON b.id = t.block
WHERE t.chain = ?1 AND t.network = ?2
  AND (t.block IS NULL OR b.archived_at IS NULL)
  AND EXISTS (SELECT 1 FROM leg l WHERE l."transaction" = t.id AND l.address = ?3)
ORDER BY t.block IS NULL, b.position, t."order", t.id"#;

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

impl TransactionRow {
    /// At most `limit` transactions of `address` in `asset`, in live blocks,
    /// after the (`position`, `order`, `txid`) key (see [`PAGE`]).
    pub(crate) fn find_page(
        connection: &mut SqliteConnection,
        address: &Address,
        asset: &Asset,
        after: (i64, i64, &str),
        limit: i64,
    ) -> QueryResult<Vec<Self>> {
        diesel::sql_query(PAGE)
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

    /// The id of `txid` of `network`: `"{chain}.{network}.{txid}"`.
    ///
    /// # Errors
    ///
    /// When a part holds a dot.
    pub(crate) fn id(network: &Network, txid: &str) -> anyhow::Result<String> {
        key(&[network.chain.as_str(), &network.name, txid])
    }

    /// Keeps pending `transaction` with every leg (no block); a no-op when
    /// its txid is kept, pending or mined.
    ///
    /// # Errors
    ///
    /// When a value does not encode or the write fails.
    pub(crate) fn insert_pending(
        connection: &mut SqliteConnection,
        transaction: &Transaction,
    ) -> anyhow::Result<()> {
        let (state, reason) = encode_state(&transaction.state)?;
        let network = &transaction.id.network;
        let id = Self::id(network, &transaction.id.id)?;
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
        };
        let added = diesel::insert_or_ignore_into(transactions::table)
            .values(&row)
            .execute(connection)?;
        if added == 0 {
            return Ok(());
        }
        let legs = (0_u32..)
            .zip(&transaction.legs)
            .map(|(index, leg)| LegRow::new(None, &id, index, leg))
            .collect::<anyhow::Result<Vec<_>>>()?;
        LegRow::insert(connection, &legs)?;
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
        transaction: &Transaction,
        legs: impl IntoIterator<Item = (u32, &'l Leg)>,
    ) -> anyhow::Result<()> {
        let (state, reason) = encode_state(&transaction.state)?;
        let network = &transaction.id.network;
        let id = Self::id(network, &transaction.id.id)?;
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
        };
        diesel::insert_into(transactions::table)
            .values(&row)
            .on_conflict(transactions::id)
            .do_update()
            .set(&row)
            .execute(connection)?;
        let rows = legs
            .into_iter()
            .map(|(index, leg)| LegRow::new(Some(block), &id, index, leg))
            .collect::<anyhow::Result<Vec<_>>>()?;
        LegRow::replace(connection, block, &id, &rows)?;
        Ok(())
    }

    /// Every pending or live mined transaction with a leg of `address` (see [`FOR_ADDRESS`]).
    pub(crate) fn find_for_address(
        connection: &mut SqliteConnection,
        address: &Address,
    ) -> QueryResult<Vec<Self>> {
        diesel::sql_query(FOR_ADDRESS)
            .bind::<Text, _>(address.network.chain.as_str())
            .bind::<Text, _>(&address.network.name)
            .bind::<Text, _>(&address.value)
            .load(connection)
    }

    /// The transaction of this row with `legs` (its own, in order).
    ///
    /// # Errors
    ///
    /// When a stored value does not decode.
    pub(crate) fn transaction(
        self,
        network: &Network,
        legs: Vec<LegFacts>,
    ) -> anyhow::Result<Transaction> {
        Ok(Transaction {
            id: TransactionId::new(network.clone(), self.txid),
            state: decode_state(self.state, self.reason.as_deref())?,
            order: self.order.map(uint).transpose()?,
            legs: legs
                .into_iter()
                .map(|leg| leg.leg(network))
                .collect::<anyhow::Result<Vec<Leg>>>()?,
            bytes: self.bytes,
        })
    }
}

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
