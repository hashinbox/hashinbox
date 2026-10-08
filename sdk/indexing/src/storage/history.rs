//! One page of a holding's history as the shipped stores answer it ([`Page`]),
//! and completed by the blocks an index holds above their line ([`History`]).

use crate::{
    Address, BlockHeight, BlockRef, Checkpointed, Holder, IndexError, IndexErrorKind, Sample,
    Status, Transaction, TransactionRef, ValueMovement,
};

/// A transaction's place in an address's history:
/// `(height, transaction order, transaction id)`, all functions of the block,
/// so a page resumes after it however far the checkpoint moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Position {
    /// The including block's produced height.
    pub height: BlockHeight,
    /// The transaction's order inside its block.
    pub order: u64,
    /// The transaction itself.
    pub transaction: TransactionRef,
}

/// A history transaction's status relative to its chain's confirmed line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Standing {
    /// Above the confirmed line; it may still revert.
    Pending {
        /// The including block.
        block: BlockRef,
        /// Blocks from it to the newest known, inclusive.
        confirmations: u64,
    },
    /// At or below the confirmed line; it will not revert.
    Confirmed {
        /// The including block.
        block: BlockRef,
        /// Blocks from it to the newest known, inclusive.
        confirmations: u64,
    },
    /// Included but failed.
    Failed {
        /// The including block.
        block: BlockRef,
        /// The chain's reason, when it gives one.
        reason: Option<String>,
    },
}

/// One transaction of a page, with its standing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The transaction, with only the holding's own movements.
    pub transaction: Transaction,
    /// Where it stands against the line.
    pub standing: Standing,
}

/// One page of a holding's history, both sides of the line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct History {
    /// The confirmed line the page was read at.
    pub checkpoint: Option<BlockRef>,
    /// Confirmed transactions, ascending, append-only.
    pub transactions: Vec<Entry>,
    /// Transactions above the line, first page only.
    pub pending: Vec<Entry>,
    /// Where the next page resumes.
    pub next: Option<Position>,
    /// How many blocks the address's inspection is behind `checkpoint`
    /// ([`crate::Inspection::behind`]).
    pub behind: u64,
}

/// The confirmed half of a page, as one snapshot of a store holds it.
///
/// A transaction is on the page when the holder's address is an endpoint of
/// one of its movements in the holder's asset, or paid its fee and either it
/// failed or the asset is the fee asset; it carries only those movements.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    checkpoint: Option<BlockRef>,
    transactions: Vec<Transaction>,
    next: Option<Position>,
    behind: u64,
}

impl Checkpointed for Page {
    fn checkpoint(&self) -> Option<&BlockRef> {
        self.checkpoint.as_ref()
    }
}

impl Page {
    /// The page one snapshot read: `matches` holds up to `limit + 1`
    /// transactions after the cursor, the extra one only proving there is more.
    #[cfg(any(feature = "sqlite", feature = "memory"))]
    pub(crate) fn new(
        checkpoint: Option<BlockRef>,
        mut matches: Vec<Transaction>,
        limit: usize,
        behind: u64,
    ) -> Self {
        let more = matches.len() > limit;
        matches.truncate(limit);
        let next = matches.last().filter(|_| more).map(|last| Position {
            height: last.block().height,
            order: last.order,
            transaction: last.id.clone(),
        });
        Self {
            checkpoint,
            transactions: matches,
            next,
            behind,
        }
    }

    /// The page with read-time standings, and on the `first` page the matching
    /// transactions of `sample`'s blocks above this read's line.
    ///
    /// Sample the pending blocks *before* the store read, so a commit landing
    /// in between is listed once.
    ///
    /// # Errors
    ///
    /// A terminal `Store` error for a transaction above its checkpoint.
    pub fn completed(
        self,
        of: &dyn Holder,
        first: bool,
        sample: Sample,
    ) -> Result<History, IndexError> {
        let blocks = sample.above(&self);
        // Confirmations count from the newest block anything is known about.
        let tip = blocks
            .last()
            .map(|held| held.block.reference().clone())
            .or_else(|| self.checkpoint.clone());
        let pending = if first {
            blocks
                .iter()
                .flat_map(|facts| facts.block.transactions())
                .filter_map(|transaction| selected(of, transaction.clone()))
                .map(|transaction| entry(transaction, tip.as_ref(), false))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        let transactions = self
            .transactions
            .into_iter()
            .map(|transaction| entry(transaction, tip.as_ref(), true))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(History {
            checkpoint: self.checkpoint,
            transactions,
            pending,
            next: self.next,
            behind: self.behind,
        })
    }
}

/// `transaction` as `of`'s page shows it: `None` when it does not belong on
/// the page, otherwise with only its movements of `of`'s asset that `of`'s
/// address is an endpoint of.
pub(crate) fn selected(of: &dyn Holder, mut transaction: Transaction) -> Option<Transaction> {
    let moves = |movement: &ValueMovement| {
        let endpoint = |address: Option<&Address>| address == Some(of.address());
        movement.asset == *of.asset() && (endpoint(movement.from()) || endpoint(movement.to()))
    };
    let moved = transaction.movements.iter().any(moves);
    if !moved && !charges(of, &transaction) {
        return None;
    }
    transaction.movements.retain(moves);
    Some(transaction)
}

/// Whether `of`'s address paid `transaction`'s fee and the fee alone puts it
/// on the page: it failed, or `of`'s asset is the fee asset.
pub(crate) fn charges(of: &dyn Holder, transaction: &Transaction) -> bool {
    let Some(fee) = &transaction.fee else {
        return false;
    };
    let failed = matches!(transaction.status, Status::Failed { .. });
    fee.payer.as_ref() == Some(of.address()) && (failed || fee.asset == *of.asset())
}

/// `transaction` with its standing below (`committed`) or above the line,
/// confirmations counted from `tip`.
fn entry(
    transaction: Transaction,
    tip: Option<&BlockRef>,
    committed: bool,
) -> Result<Entry, IndexError> {
    let block = match transaction.status.clone() {
        Status::Included { block } => block,
        Status::Failed { block, reason } => {
            return Ok(Entry {
                transaction,
                standing: Standing::Failed { block, reason },
            });
        }
    };
    let beyond = || {
        IndexError::new(
            IndexErrorKind::Store,
            "history contains a transaction beyond its checkpoint",
            false,
        )
    };
    let tip = tip.ok_or_else(|| {
        IndexError::new(
            IndexErrorKind::Store,
            "history exists without a checkpoint",
            false,
        )
    })?;
    let confirmations = tip
        .height
        .0
        .checked_sub(block.height.0)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(beyond)?;
    let standing = if committed {
        Standing::Confirmed {
            block,
            confirmations,
        }
    } else {
        Standing::Pending {
            block,
            confirmations,
        }
    };
    Ok(Entry {
        transaction,
        standing,
    })
}
