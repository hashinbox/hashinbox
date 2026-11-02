//! A network's chain as this server keeps it: kept blocks with their kept
//! transactions and legs (`block`, `transaction`, `leg` models), and its
//! pending transactions (the mempool as this server knows it). Each read is
//! one snapshot.

use diesel::SqliteConnection;

use crate::{
    db::Db,
    models::{self, Legs, block, leg, transaction},
};

/// One kept transaction with its kept legs, in order.
pub(crate) struct Transaction {
    /// Its row.
    pub(crate) row: transaction::Transaction,
    /// Its kept legs.
    pub(crate) legs: Legs,
}

impl Transaction {
    /// `row` with its kept legs.
    fn read(
        connection: &mut SqliteConnection,
        row: transaction::Transaction,
    ) -> anyhow::Result<Self> {
        let legs = leg::db::find_for(connection, &row.id)?;
        Ok(Self { row, legs })
    }
}

/// One kept block with its kept transactions, in block order.
pub(crate) struct Block {
    /// Its row.
    pub(crate) row: models::Block,
    /// Its kept transactions.
    pub(crate) transactions: Vec<Transaction>,
}

impl Block {
    /// `row` with its kept transactions.
    fn read(connection: &mut SqliteConnection, row: models::Block) -> anyhow::Result<Self> {
        let transactions = transaction::db::find_in(connection, &[row.id.as_str()])?
            .into_iter()
            .map(|tx| Transaction::read(connection, tx))
            .collect::<anyhow::Result<_>>()?;
        Ok(Self { row, transactions })
    }
}

/// A network's last kept blocks and its pending transactions.
pub(crate) struct Tail {
    /// The last kept blocks, latest first.
    pub(crate) blocks: Vec<Block>,
    /// The pending transactions, oldest sent first.
    pub(crate) mempool: Vec<Transaction>,
}

/// The last `limit` kept blocks of `network` and its pending transactions.
///
/// # Errors
///
/// The read.
pub(crate) async fn latest(db: &Db, network: base::Network, limit: i64) -> anyhow::Result<Tail> {
    db.read(move |connection| {
        let blocks = block::db::find_latest(connection, &network, limit)?
            .into_iter()
            .map(|row| Block::read(connection, row))
            .collect::<anyhow::Result<_>>()?;
        let mempool = transaction::db::find_all_pending(connection, &network)?
            .into_iter()
            .map(|tx| Transaction::read(connection, tx))
            .collect::<anyhow::Result<_>>()?;
        Ok(Tail { blocks, mempool })
    })
    .await
}

/// The live block of `network` at `position`, if one is kept.
///
/// # Errors
///
/// The read.
pub(crate) async fn at(
    db: &Db,
    network: base::Network,
    position: i64,
) -> anyhow::Result<Option<Block>> {
    db.read(move |connection| {
        block::db::find_live(connection, &network, position)?
            .map(|row| Block::read(connection, row))
            .transpose()
    })
    .await
}
