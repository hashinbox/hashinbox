//! One page of a holding's history on both sides of its chain's confirmed
//! line, read from indexing's rows (the `history_entry` view and `ledger`) and
//! completed on the first page by the blocks its index holds above that line.

use std::collections::BTreeMap;

use base::{Address, AssetId, BlockHeight, BlockRef, Holder, MovementKind, TransactionRef};
use indexing::{
    Checkpointed, IndexError, IndexErrorKind, MovementId, NetworkFee, Sample, Status, Transaction,
    ValueMovement,
};
use sqlx::{Row as _, SqliteConnection, sqlite::SqliteRow};

use super::{Block, Checkpoint, Observation};

/// Fewest rows one query reads while filling a page.
const MIN_CHUNK: usize = 8;

/// A transaction's place in an address's history:
/// `(height, transaction order, transaction id)`, all functions of the block,
/// so a page resumes after it however far the checkpoint moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Position {
    pub(crate) height: BlockHeight,
    pub(crate) order: u64,
    pub(crate) transaction: TransactionRef,
}

/// A history transaction's status relative to its chain's confirmed line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Standing {
    /// Above the confirmed line; it may still revert.
    Pending { block: BlockRef, confirmations: u64 },
    /// At or below the confirmed line; it will not revert.
    Confirmed { block: BlockRef, confirmations: u64 },
    /// Included but failed.
    Failed {
        block: BlockRef,
        reason: Option<String>,
    },
}

/// One transaction of a page, with its standing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) transaction: Transaction,
    pub(crate) standing: Standing,
}

/// One page of a holding's history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct History {
    /// The confirmed line the page was read at.
    pub(crate) checkpoint: Option<BlockRef>,
    /// Confirmed transactions, ascending, append-only.
    pub(crate) transactions: Vec<Entry>,
    /// Transactions above the line, first page only.
    pub(crate) pending: Vec<Entry>,
    /// Where the next page resumes.
    pub(crate) next: Option<Position>,
    /// How many blocks the address's inspection is behind `checkpoint`.
    pub(crate) behind: u64,
}

/// The confirmed half of a page, as one snapshot holds it.
pub(crate) struct Page {
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
    /// Up to `limit` of `of`'s confirmed transactions after `after`, read
    /// inside the snapshot `connection` holds.
    ///
    /// A transaction is on the page when `of`'s address is an endpoint of one
    /// of its movements in `of`'s asset, or paid its fee and either it failed
    /// or the asset is the fee asset; it carries only those movements.
    ///
    /// # Errors
    ///
    /// A `Store` error when a read fails or a row is malformed.
    pub(crate) async fn read(
        connection: &mut SqliteConnection,
        of: &dyn Holder,
        after: Option<&Position>,
        limit: usize,
    ) -> Result<Self, IndexError> {
        let address = of.address();
        let Checkpoint(checkpoint) = Checkpoint::get(connection, &address.network).await?;
        let behind = Observation::find_by_address(connection, address)
            .await?
            .behind(checkpoint.as_ref());
        let rows = limit.saturating_add(1);
        let chunk = rows.max(MIN_CHUNK);
        let mut cursor = after.map_or((-1, -1, String::new()), |after| {
            (
                i64::try_from(after.height.0).unwrap_or(i64::MAX),
                i64::try_from(after.order).unwrap_or(i64::MAX),
                after.transaction.id.as_str().to_owned(),
            )
        });
        let mut matches = Vec::new();
        loop {
            let entries = Self::find_after(connection, address, &cursor, chunk).await?;
            let exhausted = entries.len() < chunk;
            let (Some(first), Some(last)) = (entries.first(), entries.last()) else {
                break;
            };
            let span = (first.block().height.0, last.block().height.0);
            let mut movements = Self::find_movements(connection, of, span).await?;
            for mut transaction in entries {
                cursor = (
                    i64::try_from(transaction.block().height.0).unwrap_or(i64::MAX),
                    i64::try_from(transaction.order).unwrap_or(i64::MAX),
                    transaction.id.id.as_str().to_owned(),
                );
                transaction.movements = movements
                    .remove(transaction.id.id.as_str())
                    .unwrap_or_default();
                let kept = !transaction.movements.is_empty() || charges(of, &transaction);
                matches.extend(kept.then_some(transaction));
            }
            if exhausted || matches.len() >= rows {
                break;
            }
        }
        let more = matches.len() > limit;
        matches.truncate(limit);
        let next = matches.last().filter(|_| more).map(|last| Position {
            height: last.block().height,
            order: last.order,
            transaction: last.id.clone(),
        });
        Ok(Self {
            checkpoint,
            transactions: matches,
            next,
            behind,
        })
    }

    /// The page with read-time standings, and on the first page the matching
    /// transactions of `sample`'s blocks above this read's line.
    ///
    /// # Errors
    ///
    /// A terminal `Store` error for a transaction above its checkpoint.
    pub(crate) fn completed(
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
                .filter_map(|transaction| select(of, transaction.clone()))
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

    /// Up to `limit` of `address`'s `history_entry` rows after `cursor`, in
    /// native block order, their movements not attached.
    async fn find_after(
        connection: &mut SqliteConnection,
        address: &Address,
        cursor: &(i64, i64, String),
        limit: usize,
    ) -> Result<Vec<Transaction>, IndexError> {
        let rows = sqlx::query(
            "SELECT * FROM history_entry
             WHERE chain = ?1 AND network = ?2 AND address = ?3
               AND (block_height, transaction_order, transaction_id) > (?4, ?5, ?6)
             ORDER BY block_height, transaction_order, transaction_id LIMIT ?7",
        )
        .bind(address.network.chain.as_str())
        .bind(&address.network.name)
        .bind(&address.value)
        .bind(cursor.0)
        .bind(cursor.1)
        .bind(&cursor.2)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(connection)
        .await
        .map_err(|_| IndexError::new(IndexErrorKind::Store, "failed to read history", true))?;
        rows.iter()
            .map(|row| Self::transaction(row, address))
            .collect()
    }

    /// One `history_entry` row as the transaction it places.
    fn transaction(row: &SqliteRow, address: &Address) -> Result<Transaction, IndexError> {
        let network = &address.network;
        let id: String = row.try_get("transaction_id").map_err(|_| malformed())?;
        let order: i64 = row.try_get("transaction_order").map_err(|_| malformed())?;
        let execution: i64 = row.try_get("execution_status").map_err(|_| malformed())?;
        let reason: Option<String> = row.try_get("failure_reason").map_err(|_| malformed())?;
        let fee_asset: Option<String> = row.try_get("fee_asset").map_err(|_| malformed())?;
        let fee_amount: Option<String> = row.try_get("fee_amount").map_err(|_| malformed())?;
        let fee_payer: Option<String> = row.try_get("fee_payer").map_err(|_| malformed())?;
        let block = Block::stored(row)?;
        let status = match execution {
            0 => Status::Included { block },
            1 => Status::Failed { block, reason },
            _ => return Err(malformed()),
        };
        let fee = match (fee_asset, fee_amount) {
            (Some(asset), Some(amount)) => Some(NetworkFee {
                asset: AssetId {
                    chain: network.chain.clone(),
                    asset,
                },
                amount: amount.parse().map_err(|_| malformed())?,
                payer: fee_payer.map(|value| Address::new(network.clone(), value)),
            }),
            (None, None) => None,
            _ => return Err(malformed()),
        };
        Ok(Transaction {
            id: TransactionRef::new(network.clone(), id),
            movements: Vec::new(),
            fee,
            order: u64::try_from(order).map_err(|_| malformed())?,
            status,
        })
    }

    /// `of`'s movements in its asset between two heights, inclusive, grouped
    /// by transaction in stored order. The two rows a same-address transfer
    /// wrote, a credit and a debit, are folded back into its one movement.
    async fn find_movements(
        connection: &mut SqliteConnection,
        of: &dyn Holder,
        (lower, upper): (u64, u64),
    ) -> Result<BTreeMap<String, Vec<ValueMovement>>, IndexError> {
        let address = of.address();
        let rows = sqlx::query(
            "SELECT transaction_id, movement_id, direction, kind, amount, counterparty
             FROM ledger
             WHERE chain = ?1 AND network = ?2 AND address = ?3 AND asset = ?4
               AND block_height >= ?5 AND block_height <= ?6
               AND transaction_id <> '' AND kind < 5
             ORDER BY block_height, transaction_id, movement_order, movement_id",
        )
        .bind(address.network.chain.as_str())
        .bind(&address.network.name)
        .bind(&address.value)
        .bind(&of.asset().asset)
        .bind(i64::try_from(lower).unwrap_or(i64::MAX))
        .bind(i64::try_from(upper).unwrap_or(i64::MAX))
        .fetch_all(connection)
        .await
        .map_err(|_| IndexError::new(IndexErrorKind::Store, "failed to read movements", true))?;
        let mut grouped: BTreeMap<String, Vec<ValueMovement>> = BTreeMap::new();
        let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();
        for row in rows {
            let transaction: String = row.try_get("transaction_id").map_err(|_| malformed())?;
            let movement: String = row.try_get("movement_id").map_err(|_| malformed())?;
            let direction: i64 = row.try_get("direction").map_err(|_| malformed())?;
            let kind = match row.try_get::<i64, _>("kind").map_err(|_| malformed())? {
                0 => MovementKind::Transfer,
                1 => MovementKind::Input,
                2 => MovementKind::Output,
                3 => MovementKind::Mint,
                4 => MovementKind::Burn,
                _ => return Err(malformed()),
            };
            let amount: String = row.try_get("amount").map_err(|_| malformed())?;
            let counterparty: Option<String> =
                row.try_get("counterparty").map_err(|_| malformed())?;
            let mine = Some(address.clone());
            let other = counterparty.map(|value| Address::new(address.network.clone(), value));
            let key = (transaction.clone(), movement.clone());
            let held = grouped.entry(transaction).or_default();
            let (from, to, at) = match (seen.get(&key), direction) {
                // The other direction of the same movement: a self-transfer.
                (Some(at), _) => (mine.clone(), mine, Some(*at)),
                (None, 0) => (other, mine, None),
                (None, 1) => (mine, other, None),
                _ => return Err(malformed()),
            };
            let value = ValueMovement::stored(
                MovementId(movement),
                of.asset().clone(),
                amount.parse().map_err(|_| malformed())?,
                kind,
                from,
                to,
            )?;
            match at {
                Some(at) => held[at] = value,
                None => {
                    seen.insert(key, held.len());
                    held.push(value);
                }
            }
        }
        Ok(grouped)
    }
}

/// `transaction` as `of`'s page shows it: `None` when it does not belong on
/// the page, otherwise with only its movements of `of`'s asset that `of`'s
/// address is an endpoint of.
fn select(of: &dyn Holder, mut transaction: Transaction) -> Option<Transaction> {
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
fn charges(of: &dyn Holder, transaction: &Transaction) -> bool {
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

/// Indexed data that contradicts the schema: retrying cannot fix it.
fn malformed() -> IndexError {
    IndexError::new(
        IndexErrorKind::Store,
        "a stored transaction is malformed",
        false,
    )
}
