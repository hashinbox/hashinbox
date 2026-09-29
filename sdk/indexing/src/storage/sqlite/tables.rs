//! One network's rows: the statements every port runs, and the column mapping
//! they share.
//!
//! Separate from [`Repository`](super::Repository) because a statement runs
//! inside the blocking hop, and the repository holds the connection handle that
//! hop has already locked. What crosses into it is the network and nothing
//! else, so this type is the network plus the SQL it implies.
//!
//! This one file is where a key-value store needs a key encoder, a value
//! frame, a namespace prefix and a record mapping: a store handed bytes has to
//! build all four, and a store handed a statement needs a column list. What is
//! not about the engine survives either way — a monetary amount is canonical
//! text, and a block coordinate is checked before it is narrowed.

use std::str::FromStr;

use rusqlite::{Connection, Row, Rows, Transaction, params, types::FromSql};

use super::{IndexError, failure, store};
use crate::{
    Address, AssetId, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef,
    CanonicalTransaction, Decimal, Network, TransactionRef,
};

/// One block reference as six columns hold it.
///
/// SQLite's only integer is a signed 64-bit one, so the narrowing is named here
/// and checked in both directions rather than cast at each of the four places a
/// block is written or read.
type Coordinates = (i64, i64, Vec<u8>, Option<i64>, Option<Vec<u8>>, Option<i64>);

/// One transaction's place inside an address:
/// `(height, transaction order, transaction id)`, which is the order
/// `transaction_address` is keyed and paged in.
///
/// Every component is a function of the block alone, which is what makes a page
/// append-only: the `order` is the transaction's *native* place in its block
/// ([`crate::CanonicalTransaction::transaction_order`]), supplied by the
/// interpreter that held the whole block-ordered list, so a later watch that
/// causes more of the same block to be stored gives each transaction the very
/// place it already had rather than renumbering rows beneath a cursor somebody
/// is holding. A rank this file computed could not: it is handed the
/// transactions that touched one selected address and never the block's list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Place {
    /// Height of the including block.
    pub(super) height: i64,
    /// The transaction's native place in that block.
    pub(super) order: i64,
    /// The transaction's own identifier, or the empty sentinel for a row no
    /// transaction names.
    pub(super) id: String,
}

impl Place {
    /// Where the file holds `entry`, narrowed to what SQLite can store.
    pub(super) fn of(entry: &CanonicalTransaction) -> Result<Self, IndexError> {
        Ok(Self {
            height: Tables::stored(entry.block().height.0)?,
            order: Tables::stored(entry.transaction_order)?,
            id: entry.transaction_id.id.as_str().to_owned(),
        })
    }
}

/// Every statement one `(chain, network)` runs.
#[derive(Clone)]
pub(super) struct Tables {
    network: Network,
}

impl Tables {
    /// Binds the statements to `network`.
    pub(super) const fn new(network: Network) -> Self {
        Self { network }
    }

    /// The chain half of the `(chain, network)` every table is framed by.
    pub(super) fn chain(&self) -> &str {
        self.network.chain.as_str()
    }

    /// The network half.
    pub(super) fn name(&self) -> &str {
        &self.network.name
    }

    /// One stored address value, back in this network.
    pub(super) fn address(&self, value: String) -> Address {
        Address {
            network: self.network.clone(),
            value,
        }
    }

    /// One stored asset value, back in this network's chain.
    pub(super) fn asset(&self, value: String) -> AssetId {
        AssetId {
            chain: self.network.chain.clone(),
            asset: value,
        }
    }

    /// One stored transaction identifier, back in this network.
    pub(super) fn transaction(&self, value: String) -> TransactionRef {
        TransactionRef {
            network: self.network.clone(),
            id: value.into(),
        }
    }

    /// The network's checkpoint, or `None` before its first commit.
    pub(super) fn checkpoint(
        &self,
        connection: &Connection,
    ) -> Result<Option<BlockRef>, IndexError> {
        let held = connection
            .query_row(
                "SELECT position, height, hash, parent_position, parent_hash, timestamp
                 FROM network_checkpoint WHERE chain = ?1 AND network = ?2",
                params![self.chain(), self.name()],
                |row| Self::coordinates(row, 0),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                error => Err(failure(&error, "failed to read the checkpoint")),
            })?;
        held.map(Self::block).transpose()
    }

    /// One read snapshot of this network, and the checkpoint it is consistent
    /// with.
    ///
    /// One value, because the checkpoint read is the transaction's *first*
    /// statement and that is what fixes the snapshot every later page is read
    /// from: handing back the pair is what stops a read being written that pairs
    /// a page with a checkpoint no single snapshot held.
    pub(super) fn snapshot<'a>(
        &self,
        connection: &'a Connection,
    ) -> Result<(Transaction<'a>, Option<BlockRef>), IndexError> {
        let read = connection
            .unchecked_transaction()
            .map_err(|error| failure(&error, "failed to begin a sqlite read"))?;
        let checkpoint = self.checkpoint(&read)?;
        Ok((read, checkpoint))
    }

    /// Makes `block` the network's checkpoint, whether or not it had one.
    ///
    /// No condition and no version: [`super::Database::transaction`] already
    /// holds the file's write lock, and the read this write answers happened
    /// inside it.
    pub(super) fn set_checkpoint(
        &self,
        transaction: &Transaction<'_>,
        block: &BlockRef,
    ) -> Result<(), IndexError> {
        let (position, height, hash, parent_position, parent_hash, timestamp) =
            Self::columns(block)?;
        transaction
            .execute(
                "INSERT INTO network_checkpoint
                     (chain, network, position, height, hash, parent_position,
                      parent_hash, timestamp)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (chain, network) DO UPDATE SET
                     position = excluded.position, height = excluded.height,
                     hash = excluded.hash, parent_position = excluded.parent_position,
                     parent_hash = excluded.parent_hash, timestamp = excluded.timestamp",
                params![
                    self.chain(),
                    self.name(),
                    position,
                    height,
                    hash,
                    parent_position,
                    parent_hash,
                    timestamp
                ],
            )
            .map(drop)
            .map_err(|error| failure(&error, "failed to write the checkpoint"))
    }

    /// Removes every row of one table that belongs to this network, and counts
    /// them.
    ///
    /// One statement per table, whatever the network holds. This is the half of
    /// `docs/design/030 §8 (ex-031)` section 5.2 a key-value store cannot answer: naming
    /// every key it removes makes an offline rescan materialize one delete
    /// operation per row of the whole network before it commits any of them.
    pub(super) fn purge(
        &self,
        transaction: &Transaction<'_>,
        table: &'static str,
        narrowed: &'static str,
    ) -> Result<u64, IndexError> {
        let removed = transaction
            .execute(
                // `table` and `narrowed` are this module's own literals; nothing
                // a caller supplies reaches either.
                &format!("DELETE FROM {table} WHERE chain = ?1 AND network = ?2 {narrowed}"),
                params![self.chain(), self.name()],
            )
            .map_err(|error| failure(&error, "failed to purge a network"))?;
        u64::try_from(removed).map_err(|_| store("sqlite reported a negative row count"))
    }

    /// One monetary amount as the file holds it: canonical base-10 text,
    /// independent of the in-memory big-integer implementation.
    pub(super) fn amount(encoded: &str) -> Result<Decimal, IndexError> {
        let value = Decimal::from_str(encoded)
            .map_err(|_| store("stored amount is not a valid decimal"))?;
        if value.to_string() != encoded {
            return Err(store("stored amount is not canonical"));
        }
        value
            .to_atomic(value.scale())
            .map_err(|_| store("stored monetary amount must not be negative"))?;
        Ok(value)
    }

    /// One block coordinate, narrowed to what SQLite can hold.
    pub(super) fn stored(value: u64) -> Result<i64, IndexError> {
        i64::try_from(value).map_err(|_| store("a block coordinate is above the storable range"))
    }

    /// One stored block coordinate, widened back.
    pub(super) fn restored(value: i64) -> Result<u64, IndexError> {
        u64::try_from(value).map_err(|_| store("a stored block coordinate is negative"))
    }

    /// The next row of a running statement, with the read failure classified.
    pub(super) fn step<'a, 's>(rows: &'a mut Rows<'s>) -> Result<Option<&'a Row<'s>>, IndexError> {
        rows.next()
            .map_err(|error| failure(&error, "failed to read a row"))
    }

    /// One column of a row, with the read failure classified.
    pub(super) fn column<T: FromSql>(row: &Row<'_>, index: usize) -> Result<T, IndexError> {
        row.get(index)
            .map_err(|error| failure(&error, "failed to read a column"))
    }

    /// One amount column of a row, held to [`Self::amount`]'s rules.
    pub(super) fn decimal(row: &Row<'_>, index: usize) -> Result<Decimal, IndexError> {
        Self::amount(&Self::column::<String>(row, index)?)
    }

    /// Six consecutive columns, starting at `first`, as one block reference.
    pub(super) fn coordinates(row: &Row<'_>, first: usize) -> rusqlite::Result<Coordinates> {
        Ok((
            row.get(first)?,
            row.get(first + 1)?,
            row.get(first + 2)?,
            row.get(first + 3)?,
            row.get(first + 4)?,
            row.get(first + 5)?,
        ))
    }

    /// The block those six columns describe.
    pub(super) fn block(stored: Coordinates) -> Result<BlockRef, IndexError> {
        let (position, height, hash, parent_position, parent_hash, timestamp) = stored;
        let parent = match (parent_position, parent_hash) {
            (Some(position), Some(hash)) => Some(BlockParent {
                position: BlockPosition(Self::restored(position)?),
                hash: BlockHash(hash),
            }),
            (None, None) => None,
            _ => return Err(store("a stored block parent is half written")),
        };
        Ok(BlockRef {
            position: BlockPosition(Self::restored(position)?),
            height: BlockHeight(Self::restored(height)?),
            hash: BlockHash(hash),
            parent,
            timestamp: timestamp.map(Self::restored).transpose()?,
        })
    }

    /// The six columns `block` is written as.
    pub(super) fn columns(block: &BlockRef) -> Result<Coordinates, IndexError> {
        Ok((
            Self::stored(block.position.0)?,
            Self::stored(block.height.0)?,
            block.hash.0.clone(),
            block
                .parent
                .as_ref()
                .map(|parent| Self::stored(parent.position.0))
                .transpose()?,
            block.parent.as_ref().map(|parent| parent.hash.0.clone()),
            block.timestamp.map(Self::stored).transpose()?,
        ))
    }
}
