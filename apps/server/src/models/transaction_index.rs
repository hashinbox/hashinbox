//! `transaction_index`: one sent transaction being looked for in the blocks
//! after its broadcast (design 085), with the position it is indexed
//! through and the one it is found at (design 087).

use std::collections::BTreeSet;

use base::{Confirmed, IndexState, Network, Progress};
use diesel::prelude::*;

use super::{
    Block, int,
    schema::{network_index, transaction_index, transactions},
    transaction::TransactionId,
    uint,
    wallet_index::db::{code, state},
};

/// Why a transaction index was given up, or its pending transaction
/// forgotten: the index's `reason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    /// Every node refused it, or it was never sent after one that was.
    Refused,
    /// Its index (a wallet's or its own) jumped past a gap, where it may
    /// have mined; or its own was given up, or found it mined inside one.
    Jump,
    /// A bump replaced it at a higher fee, or a cancel; its own index
    /// watches on while its job runs.
    Replaced,
}

impl Why {
    /// As stored.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Refused => "refused",
            Self::Jump => "jump",
            Self::Replaced => "replaced",
        }
    }
}

/// One sent transaction being looked for, as every read answers it: its
/// `transaction_index` row, its transaction row's state (none once
/// forgotten) and its network's confirmed position.
#[derive(Queryable)]
pub(crate) struct TransactionIndex {
    id: TransactionId,
    txid: String,
    broadcasted_at: i64,
    expires_height: Option<i64>,
    indexed_at: Option<i64>,
    found_at: Option<i64>,
    state: i32,
    reason: Option<String>,
    transaction_state: Option<i32>,
    confirmed: Option<i64>,
}

impl TryFrom<TransactionIndex> for base::TransactionIndex {
    type Error = anyhow::Error;

    fn try_from(index: TransactionIndex) -> anyhow::Result<Self> {
        Ok(Self {
            transaction: base::TransactionId::new(index.id.network, index.txid),
            broadcasted_at: uint(index.broadcasted_at)?,
            expires_height: index.expires_height.map(uint).transpose()?,
            found: index.found_at.map(uint).transpose()?,
            // Found, its transaction row is mined with its outcome (the block
            // read, or the `Confirmer`'s answer).
            reverted: index.found_at.is_some() && index.transaction_state == Some(2),
            progress: Progress {
                indexed_at: index.indexed_at.map(uint).transpose()?,
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

    /// Every `transaction_index` row with its transaction row's state and
    /// its network's confirmed position (none while its tip is unknown),
    /// selected as [`TransactionIndex`].
    macro_rules! read {
        () => {
            transaction_index::table
                .left_join(transactions::table.on(transactions::id.eq(transaction_index::id)))
                .left_join(
                    network_index::table.on(network_index::chain
                        .eq(transaction_index::chain)
                        .and(network_index::network.eq(transaction_index::network))),
                )
                .select((
                    transaction_index::id,
                    transaction_index::txid,
                    transaction_index::broadcasted_at,
                    transaction_index::expires_height,
                    transaction_index::indexed_at,
                    transaction_index::found_at,
                    transaction_index::state,
                    transaction_index::reason,
                    transactions::state.nullable(),
                    network_index::confirmed.nullable(),
                ))
        };
    }

    /// The rows of `network`.
    macro_rules! of {
        ($network:expr) => {
            transaction_index::table
                .filter(transaction_index::chain.eq($network.chain.as_str()))
                .filter(transaction_index::network.eq($network.name.as_str()))
        };
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

    /// Every index of `network`.
    ///
    /// # Errors
    ///
    /// When a read fails or a stored value does not decode.
    pub(crate) fn find_all(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> anyhow::Result<Vec<base::TransactionIndex>> {
        read!()
            .filter(transaction_index::chain.eq(network.chain.as_str()))
            .filter(transaction_index::network.eq(&network.name))
            .order(transaction_index::id)
            .load::<TransactionIndex>(connection)?
            .into_iter()
            .map(base::TransactionIndex::try_from)
            .collect()
    }

    /// Index `id` (its transaction's id), if kept.
    ///
    /// # Errors
    ///
    /// When a read fails or a stored value does not decode.
    pub(crate) fn find(
        connection: &mut SqliteConnection,
        id: impl AsRef<str>,
    ) -> anyhow::Result<Option<base::TransactionIndex>> {
        read!()
            .filter(transaction_index::id.eq(id.as_ref()))
            .first::<TransactionIndex>(connection)
            .optional()?
            .map(base::TransactionIndex::try_from)
            .transpose()
    }

    /// Moves every index `block` (kept) applies to onto its position, in
    /// one statement: indexing and not found, broadcast at or below it, and
    /// indexed through its parent's position (any position from the
    /// parent's up to the block's), or unstarted with that parent below
    /// their start (none: the chain's first). How many.
    pub(crate) fn set_indexed_at(
        connection: &mut SqliteConnection,
        block: &Block,
    ) -> QueryResult<usize> {
        diesel::update(
            transaction_index::table
                .filter(transaction_index::chain.eq(&block.chain))
                .filter(transaction_index::network.eq(&block.network))
                .filter(transaction_index::state.eq(code(IndexState::Indexing)))
                .filter(transaction_index::found_at.is_null())
                .filter(transaction_index::broadcasted_at.le(block.position))
                .filter(
                    transaction_index::indexed_at
                        .ge(block.parent_position)
                        .and(transaction_index::indexed_at.lt(block.position))
                        .or(transaction_index::indexed_at.is_null().and(
                            transaction_index::broadcasted_at
                                .gt(block.parent_position.unwrap_or(-1)),
                        )),
                ),
        )
        .set(transaction_index::indexed_at.eq(block.position))
        .execute(connection)
    }

    /// Sets found at `block`'s position the indexes moved onto it
    /// ([`set_indexed_at`], first) whose transaction is in it (`ids`), in
    /// one statement; their ids, the answer. Expiry is not judged here: one
    /// not found reads on.
    pub(crate) fn set_found_at(
        connection: &mut SqliteConnection,
        block: &Block,
        ids: &[&str],
    ) -> QueryResult<BTreeSet<String>> {
        let found: Vec<String> = diesel::update(
            transaction_index::table
                .filter(transaction_index::id.eq_any(ids))
                .filter(transaction_index::indexed_at.eq(block.position))
                .filter(transaction_index::found_at.is_null()),
        )
        .set(transaction_index::found_at.eq(block.position))
        .returning(transaction_index::id)
        .get_results(connection)?;
        Ok(found.into_iter().collect())
    }

    /// Caps every index of `network` at `to` (a reorg's): one indexed
    /// through a position above it is indexed through `to`
    /// (`MIN(indexed_at, to)`), or unset when `to` is below its start.
    pub(crate) fn cap_indexed_at(
        connection: &mut SqliteConnection,
        network: &Network,
        to: i64,
    ) -> QueryResult<()> {
        let above = of!(network).filter(transaction_index::indexed_at.gt(to));
        diesel::update(above.filter(transaction_index::broadcasted_at.le(to)))
            .set(transaction_index::indexed_at.eq(to))
            .execute(connection)?;
        diesel::update(above)
            .set(transaction_index::indexed_at.eq(None::<i64>))
            .execute(connection)?;
        Ok(())
    }

    /// Clears every index of `network` found at or above `from` (a
    /// reorg's): it reads on. How many.
    pub(crate) fn clear_found_from(
        connection: &mut SqliteConnection,
        network: &Network,
        from: i64,
    ) -> QueryResult<usize> {
        diesel::update(of!(network).filter(transaction_index::found_at.ge(from)))
            .set(transaction_index::found_at.eq(None::<i64>))
            .execute(connection)
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

    /// Settles index `id` as `confirmed` tells: indexed through its
    /// position, continuing from the one after, and, when mined, found at
    /// its `found` (its outcome is its transaction row's:
    /// `transaction::db::set_mined`).
    ///
    /// # Errors
    ///
    /// When a position is out of range or a write fails.
    pub(crate) fn settle(
        connection: &mut SqliteConnection,
        id: impl AsRef<str>,
        confirmed: &Confirmed,
    ) -> anyhow::Result<()> {
        let found = confirmed.found.map(int).transpose()?;
        diesel::update(transaction_index::table.find(id.as_ref()))
            .set((
                transaction_index::indexed_at.eq(int(confirmed.position)?),
                transaction_index::found_at.eq(found),
            ))
            .execute(connection)?;
        Ok(())
    }

    /// Gives up the indexes of `ids` (transaction ids), for `reason`: their
    /// transactions were forgotten. One found stays found.
    pub(crate) fn give_up(
        connection: &mut SqliteConnection,
        ids: &[impl AsRef<str>],
        reason: &str,
    ) -> QueryResult<usize> {
        let ids: Vec<&str> = ids.iter().map(AsRef::as_ref).collect();
        diesel::update(
            transaction_index::table
                .filter(transaction_index::id.eq_any(ids))
                .filter(transaction_index::found_at.is_null()),
        )
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
