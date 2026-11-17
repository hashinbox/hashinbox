//! `transaction_index`: one sent transaction being looked for in the blocks
//! after its broadcast (design 085), with the block it is indexed through
//! and the one it is found in.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context as _;
use base::{BlockHeader, IndexState, Network, Progress};
use diesel::{prelude::*, sqlite::Sqlite};

use super::{
    Block,
    block::{self as blocks, BlockId},
    int, network_index,
    schema::{block, transaction_index},
    transaction::TransactionId,
    uint,
    wallet_index::db::{code, state},
};

/// The `transaction_index` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    /// One `transaction_index` row.
    #[derive(Queryable, Selectable, Insertable)]
    #[diesel(table_name = transaction_index, check_for_backend(Sqlite))]
    struct Row {
        id: TransactionId,
        chain: String,
        network: String,
        txid: String,
        broadcasted_at: i64,
        expires_height: Option<i64>,
        indexed_at: Option<String>,
        found: Option<String>,
        state: i32,
        reason: Option<String>,
    }

    impl Row {
        /// The index, its blocks' headers read from `headers` (by id), and
        /// its network's final block `confirmed`.
        fn index(
            self,
            headers: &BTreeMap<String, BlockHeader>,
            confirmed: Option<u64>,
        ) -> anyhow::Result<base::TransactionIndex> {
            let header = |id: Option<String>| {
                id.map(|id| {
                    headers
                        .get(&id)
                        .cloned()
                        .context("an index's block is not kept")
                })
                .transpose()
            };
            Ok(base::TransactionIndex {
                transaction: base::TransactionId::new(self.id.network, self.txid),
                broadcasted_at: uint(self.broadcasted_at)?,
                expires_height: self.expires_height.map(uint).transpose()?,
                found: header(self.found)?,
                progress: Progress {
                    indexed_at: header(self.indexed_at)?,
                    state: state(self.state)?,
                    reason: self.reason,
                },
                confirmed,
            })
        }
    }

    /// `rows` as indexes, the blocks they name read in one query.
    fn indexes(
        connection: &mut SqliteConnection,
        rows: Vec<Row>,
    ) -> anyhow::Result<Vec<base::TransactionIndex>> {
        let named: Vec<&str> = rows
            .iter()
            .flat_map(|row| [&row.indexed_at, &row.found])
            .flatten()
            .map(String::as_str)
            .collect();
        let headers = blocks::db::find_by_ids(connection, &named)?
            .iter()
            .map(|block| {
                Ok((
                    String::from(block.id.clone()),
                    BlockHeader::try_from(block)?,
                ))
            })
            .collect::<anyhow::Result<_>>()?;
        // Each network's final block, as its indexer last read it.
        let mut confirmed = BTreeMap::<(String, String), Option<u64>>::new();
        for row in &rows {
            let key = (row.chain.clone(), row.network.clone());
            if confirmed.contains_key(&key) {
                continue;
            }
            let network = Network::new(base::Chain::new(&key.0), &key.1);
            let at = network_index::db::find_confirmed(connection, &network)?;
            confirmed.insert(key, at.map(uint).transpose()?);
        }
        rows.into_iter()
            .map(|row| {
                let at = confirmed
                    .get(&(row.chain.clone(), row.network.clone()))
                    .copied()
                    .flatten();
                row.index(&headers, at)
            })
            .collect()
    }

    /// Keeps `index`, not started; one already kept stays as it is.
    ///
    /// # Errors
    ///
    /// When a part of its id holds a dot, a position is out of range, or the
    /// write fails.
    pub(crate) fn insert(
        connection: &mut SqliteConnection,
        index: &base::TransactionIndex,
    ) -> anyhow::Result<()> {
        let transaction = &index.transaction;
        let row = Row {
            id: TransactionId::new(&transaction.network, &transaction.id)?,
            chain: transaction.network.chain.to_string(),
            network: transaction.network.name.clone(),
            txid: transaction.id.clone(),
            broadcasted_at: int(index.broadcasted_at)?,
            expires_height: index.expires_height.map(int).transpose()?,
            indexed_at: None,
            found: None,
            state: code(index.progress.state),
            reason: index.progress.reason.clone(),
        };
        diesel::insert_into(transaction_index::table)
            .values(&row)
            .on_conflict_do_nothing()
            .execute(connection)?;
        Ok(())
    }

    /// Every index of `network`, with the blocks it names.
    ///
    /// # Errors
    ///
    /// When a read fails or a stored value does not decode.
    pub(crate) fn find_all(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> anyhow::Result<Vec<base::TransactionIndex>> {
        let rows = transaction_index::table
            .filter(transaction_index::chain.eq(network.chain.as_str()))
            .filter(transaction_index::network.eq(&network.name))
            .order(transaction_index::id)
            .select(Row::as_select())
            .load(connection)?;
        indexes(connection, rows)
    }

    /// Index `id` (its transaction's id), if kept, with the blocks it names.
    ///
    /// # Errors
    ///
    /// When a read fails or a stored value does not decode.
    pub(crate) fn find(
        connection: &mut SqliteConnection,
        id: impl AsRef<str>,
    ) -> anyhow::Result<Option<base::TransactionIndex>> {
        let rows = transaction_index::table
            .find(id.as_ref())
            .select(Row::as_select())
            .load(connection)?;
        Ok(indexes(connection, rows)?.pop())
    }

    /// Moves every indexing index `block` (kept live, its parent's id
    /// `parent`) applies to onto it, as a wallet index moves: its start at
    /// or below it, and indexed through its parent, or unstarted with that
    /// parent below its start (none: the chain's first). Those whose
    /// transaction it holds are found in it: their ids.
    pub(crate) fn advance(
        connection: &mut SqliteConnection,
        block: &Block,
        parent: Option<&BlockId>,
    ) -> QueryResult<BTreeSet<String>> {
        let moved: Vec<String> = diesel::update(
            transaction_index::table
                .filter(transaction_index::chain.eq(&block.chain))
                .filter(transaction_index::network.eq(&block.network))
                .filter(transaction_index::state.eq(code(IndexState::Indexing)))
                // Found: it stays on its block (`Indexed::next`).
                .filter(transaction_index::found.is_null())
                .filter(transaction_index::broadcasted_at.le(block.position))
                .filter(transaction_index::indexed_at.eq(parent).or(
                    transaction_index::indexed_at.is_null().and(
                        transaction_index::broadcasted_at.gt(block.parent_position.unwrap_or(-1)),
                    ),
                )),
        )
        .set(transaction_index::indexed_at.eq(&block.id))
        .returning(transaction_index::id)
        .get_results(connection)?;
        let found: BTreeSet<String> = moved
            .into_iter()
            .filter(|id| {
                let id = id.as_str();
                block
                    .transactions
                    .iter()
                    .any(|mined| mined.id.as_ref() == id)
            })
            .collect();
        diesel::update(transaction_index::table.filter(transaction_index::id.eq_any(&found)))
            .set(transaction_index::found.eq(&block.id))
            .execute(connection)?;
        Ok(found)
    }

    /// Rewinds every index of `network` indexed through a live block at or
    /// above `from` to `below` (the live block under `from`), or unsets it
    /// when `below` is under its start; a `found` on such a block is
    /// cleared.
    pub(crate) fn rewind(
        connection: &mut SqliteConnection,
        network: &Network,
        from: i64,
        below: Option<&Block>,
    ) -> QueryResult<()> {
        let taken = block::table
            .filter(block::chain.eq(network.chain.as_str()))
            .filter(block::network.eq(&network.name))
            .filter(block::reorged_at.is_null())
            .filter(block::position.ge(from))
            .select(block::id.nullable());
        diesel::update(transaction_index::table.filter(transaction_index::found.eq_any(taken)))
            .set(transaction_index::found.eq(None::<String>))
            .execute(connection)?;
        let through = transaction_index::table.filter(transaction_index::indexed_at.eq_any(taken));
        if let Some(below) = below {
            diesel::update(through.filter(transaction_index::broadcasted_at.le(below.position)))
                .set(transaction_index::indexed_at.eq(&below.id))
                .execute(connection)?;
        }
        diesel::update(through)
            .set(transaction_index::indexed_at.eq(None::<String>))
            .execute(connection)?;
        Ok(())
    }

    /// Moves the start of `index` to `start` while it has not started.
    ///
    /// # Errors
    ///
    /// When a value is out of range or the write fails.
    pub(crate) fn move_start(
        connection: &mut SqliteConnection,
        id: impl AsRef<str>,
        start: u64,
    ) -> anyhow::Result<()> {
        diesel::update(
            transaction_index::table
                .find(id.as_ref())
                .filter(transaction_index::indexed_at.is_null()),
        )
        .set(transaction_index::broadcasted_at.eq(int(start)?))
        .execute(connection)?;
        Ok(())
    }

    /// Sets the state and reason of index `id`.
    pub(crate) fn set_state(
        connection: &mut SqliteConnection,
        id: impl AsRef<str>,
        state: IndexState,
        reason: Option<&str>,
    ) -> QueryResult<usize> {
        diesel::update(transaction_index::table.find(id.as_ref()))
            .set((
                transaction_index::state.eq(code(state)),
                transaction_index::reason.eq(reason),
            ))
            .execute(connection)
    }

    /// Settles index `id` on `block`, a final block, and `found`, the one
    /// it is mined in: it continues from the block after.
    pub(crate) fn settle(
        connection: &mut SqliteConnection,
        id: impl AsRef<str>,
        block: &Block,
        found: Option<&Block>,
    ) -> QueryResult<usize> {
        let index = transaction_index::table.find(id.as_ref());
        if let Some(found) = found {
            diesel::update(index)
                .set(transaction_index::found.eq(&found.id))
                .execute(connection)?;
        }
        diesel::update(index)
            .set(transaction_index::indexed_at.eq(&block.id))
            .execute(connection)
    }

    /// Gives up the indexes of `ids` (transaction ids), for `reason`: their
    /// transactions were forgotten.
    pub(crate) fn give_up(
        connection: &mut SqliteConnection,
        ids: &[impl AsRef<str>],
        reason: &str,
    ) -> QueryResult<usize> {
        let ids: Vec<&str> = ids.iter().map(AsRef::as_ref).collect();
        diesel::update(transaction_index::table.filter(transaction_index::id.eq_any(ids)))
            .set((
                transaction_index::state.eq(code(IndexState::NoData)),
                transaction_index::reason.eq(reason),
            ))
            .execute(connection)
    }

    /// Forgets index `id`; how many went (0 or 1).
    pub(crate) fn delete(
        connection: &mut SqliteConnection,
        id: impl AsRef<str>,
    ) -> QueryResult<usize> {
        diesel::delete(transaction_index::table.find(id.as_ref())).execute(connection)
    }
}
