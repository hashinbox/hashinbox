//! `transaction_index`: one sent transaction being looked for in the blocks
//! after its broadcast (design 085), with the block it is indexed through
//! and the one it is found in.

use std::collections::BTreeSet;

use base::{BlockHeader, IndexState, Network, Progress};
use diesel::prelude::*;

use super::{
    Block,
    block::{self as blocks, BlockId},
    int,
    schema::{block, transaction_index, transaction_index_with_blocks as with_blocks},
    transaction::TransactionId,
    uint,
    wallet_index::db::{code, state},
};

/// One sent transaction being looked for, as every read answers it: its
/// `transaction_index` row with the blocks it names (live or reorged, as
/// kept), and, from `transaction_index_with_blocks`, whether its found
/// transaction reverted and its network's confirmed position.
pub(crate) struct TransactionIndex {
    id: TransactionId,
    txid: String,
    broadcasted_at: i64,
    expires_height: Option<i64>,
    indexed_at: Option<Block>,
    found: Option<Block>,
    state: i32,
    reason: Option<String>,
    reverted: bool,
    confirmed: Option<i64>,
}

impl TryFrom<TransactionIndex> for base::TransactionIndex {
    type Error = anyhow::Error;

    fn try_from(index: TransactionIndex) -> anyhow::Result<Self> {
        let header = |block: Option<Block>| block.as_ref().map(BlockHeader::try_from).transpose();
        Ok(Self {
            transaction: base::TransactionId::new(index.id.network, index.txid),
            broadcasted_at: uint(index.broadcasted_at)?,
            expires_height: index.expires_height.map(uint).transpose()?,
            found: header(index.found)?,
            reverted: index.reverted,
            progress: Progress {
                indexed_at: header(index.indexed_at)?,
                state: state(index.state)?,
                reason: index.reason,
            },
            confirmed: index.confirmed.map(uint).transpose()?,
        })
    }
}

/// The `transaction_index` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    diesel::alias!(block as indexed_block: IndexedBlock, block as found_block: FoundBlock);

    /// What [`read!`] selects: an index's columns and the blocks it names.
    type Read = (
        (
            TransactionId,
            String,
            i64,
            Option<i64>,
            i32,
            Option<String>,
            bool,
            Option<i64>,
        ),
        Option<blocks::db::Row>,
        Option<blocks::db::Row>,
    );

    /// Every `transaction_index_with_blocks` row with the blocks it names,
    /// selected as [`Read`].
    macro_rules! read {
        () => {
            with_blocks::table
                .left_join(
                    indexed_block
                        .on(with_blocks::indexed_at.eq(indexed_block.field(block::id).nullable())),
                )
                .left_join(
                    found_block.on(with_blocks::found.eq(found_block.field(block::id).nullable())),
                )
                .select((
                    (
                        with_blocks::id,
                        with_blocks::txid,
                        with_blocks::broadcasted_at,
                        with_blocks::expires_height,
                        with_blocks::state,
                        with_blocks::reason,
                        with_blocks::reverted,
                        with_blocks::confirmed,
                    ),
                    indexed_block.fields(block::all_columns).nullable(),
                    found_block.fields(block::all_columns).nullable(),
                ))
        };
    }

    /// The index [`read!`] answered `read`.
    fn index(read: Read) -> anyhow::Result<base::TransactionIndex> {
        let (columns, indexed_at, found) = read;
        let (id, txid, broadcasted_at, expires_height, state, reason, reverted, confirmed) =
            columns;
        base::TransactionIndex::try_from(TransactionIndex {
            id,
            txid,
            broadcasted_at,
            expires_height,
            indexed_at: indexed_at.map(Block::from),
            found: found.map(Block::from),
            state,
            reason,
            reverted,
            confirmed,
        })
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
        diesel::insert_into(transaction_index::table)
            .values((
                transaction_index::id
                    .eq(TransactionId::new(&transaction.network, &transaction.id)?),
                transaction_index::chain.eq(transaction.network.chain.as_str()),
                transaction_index::network.eq(&transaction.network.name),
                transaction_index::txid.eq(&transaction.id),
                transaction_index::broadcasted_at.eq(int(index.broadcasted_at)?),
                transaction_index::expires_height.eq(index.expires_height.map(int).transpose()?),
                transaction_index::state.eq(code(index.progress.state)),
                transaction_index::reason.eq(&index.progress.reason),
            ))
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
        read!()
            .filter(with_blocks::chain.eq(network.chain.as_str()))
            .filter(with_blocks::network.eq(&network.name))
            .order(with_blocks::id)
            .load::<Read>(connection)?
            .into_iter()
            .map(index)
            .collect()
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
        read!()
            .filter(with_blocks::id.eq(id.as_ref()))
            .first::<Read>(connection)
            .optional()?
            .map(index)
            .transpose()
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
            .set((
                transaction_index::found.eq(None::<String>),
                transaction_index::found_reverted.eq(false),
            ))
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
    /// it is mined in, `reverted` there or not: it continues from the block
    /// after. Its transaction stays as kept: `found` is kept without it.
    pub(crate) fn settle(
        connection: &mut SqliteConnection,
        id: impl AsRef<str>,
        block: &Block,
        found: Option<&Block>,
        reverted: bool,
    ) -> QueryResult<usize> {
        let index = transaction_index::table.find(id.as_ref());
        if let Some(found) = found {
            diesel::update(index)
                .set((
                    transaction_index::found.eq(&found.id),
                    transaction_index::found_reverted.eq(reverted),
                ))
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
