//! `output`: one outpoint's whole life — what created it, the block that spent
//! it, and the page of the ones still unspent.
//!
//! **One table, and the property two tables cooperated on is structural.** A
//! spend is an `UPDATE` that sets `spent_block_id` and a create is an upsert
//! that fills the creation columns **only where they are null**. Filling the
//! creation of a row whose spend is already known leaves the spend set, so the
//! output is never offered again — the block that spent it is above the
//! address's anchor and no chunk will carry it again, while the block that
//! created it may still be below the checkpoint waiting to be filled in. The
//! redo of a batch whose outcome was lost writes nothing new, because the row
//! already has its creation.
//!
//! **Only required spends are remembered.** A tracked spend writes nothing, for
//! the same reason it reports nothing: no address claimed the outpoint, and the
//! backfill of the address that eventually does claim it anchors above this
//! block and so inspects this very spend again. A row per unwatched outpoint
//! spent in every block would grow with the chain, which is the one growth this
//! table must not have.

use std::collections::{BTreeMap, BTreeSet};

use sqlx::{FromRow, QueryBuilder, SqliteConnection, sqlite::SqliteRow};

use super::{
    Chunks, address_id, amount, canonical_id, column, failure, fetched, native_part, network,
    refused, restored, store,
};
use crate::{
    Address, AssetId, BlockHeight, IndexError, IndexedOutput, Network, OutputChanges, OutputId,
    OutputKey, TransactionRef,
};

/// The columns every read of an output selects: its row, and the height of
/// the block that created it.
macro_rules! joined {
    ($tail:literal) => {
        concat!(
            "SELECT o.*, b.height AS created_height
             FROM output o LEFT JOIN block b ON b.id = o.created_block_id ",
            $tail
        )
    };
}

/// What created an output: the five columns a creation fills together.
#[derive(Debug, PartialEq, Eq)]
struct Creation {
    asset: String,
    amount: String,
    evidence: Vec<u8>,
    created_block_id: String,
    coinbase: bool,
}

/// One `output` row, with its creating block's height when it has one.
///
/// A row whose spend arrived before its creation has no creation yet: the
/// spend is remembered so that filling the creation in later can never offer
/// the output again.
#[derive(Debug)]
pub(in crate::storage::sqlite) struct Output {
    id: String,
    network: Network,
    transaction_id: String,
    output_index: i64,
    address_id: Option<String>,
    creation: Option<Creation>,
    created_height: Option<i64>,
    spent_block_id: Option<String>,
}

impl FromRow<'_, SqliteRow> for Output {
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: column(row, "id")?,
            network: network(row)?,
            transaction_id: column(row, "transaction_id")?,
            output_index: column(row, "output_index")?,
            address_id: column(row, "address_id")?,
            creation: Creation::from_row(row)?,
            created_height: column(row, "created_height")?,
            spent_block_id: column(row, "spent_block_id")?,
        })
    }
}

impl Output {
    /// The id of the output `key` names: `<chain>:<network>:<tx>:<index>`.
    fn id_of(output: &OutputId) -> String {
        let network = &output.transaction.network;
        canonical_id(&[
            network.chain.as_str(),
            &network.name,
            output.transaction.id.as_str(),
            &output.index.to_string(),
        ])
    }

    /// Whether the row is still spendable: created and not spent.
    const fn live(&self) -> bool {
        self.creation.is_some() && self.spent_block_id.is_none()
    }

    /// Whether `other` records the same creation, for the same address, as
    /// this row.
    ///
    /// The comparison a re-presented creation is verified by: the spend is not
    /// part of it, because a creation filled into a row whose spend was
    /// already known is still that row.
    fn same_creation(&self, other: &Self) -> bool {
        self.creation == other.creation && self.address_id == other.address_id
    }

    /// Up to `limit` of `address`'s live outputs after the output id `after`,
    /// in id order: created, and not spent.
    /// Deletes `address_id`'s outputs in `asset`, and its spend-first
    /// tombstones (which carry no asset) when `tombstones`.
    pub(in crate::storage::sqlite) async fn delete_by_holding(
        connection: &mut SqliteConnection,
        (address_id, asset): (&str, &str),
        tombstones: bool,
    ) -> Result<(), IndexError> {
        sqlx::query(
            "DELETE FROM output WHERE address_id = ?1
               AND (asset = ?2 OR (asset IS NULL AND ?3))",
        )
        .bind(address_id)
        .bind(asset)
        .bind(tombstones)
        .execute(connection)
        .await
        .map(drop)
        .map_err(|error| failure(&error, "failed to delete outputs"))
    }

    pub(in crate::storage::sqlite) async fn find_live(
        connection: &mut SqliteConnection,
        address: &Address,
        after: &str,
        limit: usize,
    ) -> Result<Vec<Self>, IndexError> {
        let bound = i64::try_from(limit).unwrap_or(i64::MAX);
        let query = sqlx::query(joined!(
            "WHERE o.address_id = ?1 AND o.spent_block_id IS NULL
               AND o.asset IS NOT NULL AND o.id > ?2
             ORDER BY o.id LIMIT ?3"
        ))
        .bind(address_id(&address.network, &address.value))
        .bind(after)
        .bind(bound);
        let rows: Vec<Self> = fetched(connection, query, "failed to read live outputs").await?;
        if rows.iter().any(|row| !row.live()) {
            return Err(store("a stored live output is not spendable"));
        }
        Ok(rows)
    }

    /// Writes the block `created_block_id` created's outputs, verifying any the
    /// index already holds rather than refusing them: a redone batch
    /// re-presents its own, and one outpoint cannot honestly have two
    /// contents.
    ///
    /// An output some earlier spend already named keeps that spend. The upsert
    /// fills the creation columns only where they are null, so discovering a
    /// creation below the checkpoint can never erase a spending above it, and
    /// the redo of a lost batch is a no-op rather than a second create.
    pub(in crate::storage::sqlite) async fn insert_created(
        connection: &mut SqliteConnection,
        created_block_id: &str,
        outputs: &[IndexedOutput],
    ) -> Result<(), IndexError> {
        let rows: Vec<_> = outputs
            .iter()
            .map(|output| Self::created(output, created_block_id))
            .collect();
        let written = Self::upsert_many(connection, &rows).await?;
        if usize::try_from(written).ok() == Some(rows.len()) {
            return Ok(());
        }
        // Some row was neither written nor filled in: compare every row with
        // the creation the file holds, which for a row this very statement
        // wrote is its own.
        let held = Self::find_created(connection, &rows).await?;
        let held: BTreeMap<_, _> = held.iter().map(|row| (row.id.as_str(), row)).collect();
        let agrees = |row: &Self| {
            held.get(row.id.as_str())
                .is_some_and(|held| held.same_creation(row))
        };
        if rows.iter().all(agrees) {
            return Ok(());
        }
        Err(store("a backfilled output disagrees with the stored one"))
    }

    /// Writes or fills in the creation of every row, many to a statement,
    /// keeping a spend a row already holds; the number of rows written or
    /// filled, short of `rows` exactly when the file already held a creation
    /// for some.
    async fn upsert_many(
        connection: &mut SqliteConnection,
        rows: &[Self],
    ) -> Result<u64, IndexError> {
        Chunks::new(rows, "failed to write outputs")
            .execute(connection, |chunk| {
                let mut statement = QueryBuilder::new(
                    "INSERT INTO output
                         (id, chain, network, transaction_id, output_index, address_id,
                          asset, amount, evidence, created_block_id, coinbase) ",
                );
                statement.push_values(chunk, |mut values, row: &Self| {
                    let creation = row.creation.as_ref();
                    values
                        .push_bind(row.id.as_str())
                        .push_bind(row.network.chain.as_str())
                        .push_bind(row.network.name.as_str())
                        .push_bind(row.transaction_id.as_str())
                        .push_bind(row.output_index)
                        .push_bind(row.address_id.as_deref())
                        .push_bind(creation.map(|creation| creation.asset.as_str()))
                        .push_bind(creation.map(|creation| creation.amount.as_str()))
                        .push_bind(creation.map(|creation| creation.evidence.as_slice()))
                        .push_bind(creation.map(|creation| creation.created_block_id.as_str()))
                        .push_bind(creation.map(|creation| i64::from(creation.coinbase)));
                });
                statement.push(
                    " ON CONFLICT (id)
                     DO UPDATE SET asset = excluded.asset, amount = excluded.amount,
                         evidence = excluded.evidence,
                         created_block_id = excluded.created_block_id,
                         coinbase = excluded.coinbase
                     WHERE output.asset IS NULL",
                );
                statement
            })
            .await
    }

    /// The created rows the file holds under the ids of `rows`, spent or not,
    /// many to a statement, in no particular order.
    ///
    /// Not narrowed to the unspent set: a row answered here is the one a
    /// re-presented creation is compared against, and narrowing would make the
    /// redo of a lost batch read as a disagreement.
    async fn find_created(
        connection: &mut SqliteConnection,
        rows: &[Self],
    ) -> Result<Vec<Self>, IndexError> {
        Chunks::new(rows, "failed to look up outputs")
            .fetch(connection, |chunk| {
                let mut statement =
                    QueryBuilder::new(joined!("WHERE o.asset IS NOT NULL AND o.id IN ("));
                let mut ids = statement.separated(", ");
                for row in chunk {
                    ids.push_bind(row.id.as_str());
                }
                statement.push(")");
                statement
            })
            .await
    }

    /// Marks every spent output the index holds as spent in `spent_block_id`,
    /// recording and reporting the required spends whose outputs it does not
    /// hold.
    ///
    /// A required spend of an output this network never indexed is the normal
    /// outcome of watching an already-funded address, so it is reported rather
    /// than refused. What *is* recorded is the spend itself, against an
    /// outpoint whose creation is still unknown, because that creation may
    /// arrive from below.
    ///
    /// Spends are taken in order, required before tracked, and only the first
    /// spend of an outpoint can find it unspent: a repeat finds it spent, as it
    /// would have one statement at a time.
    pub(in crate::storage::sqlite) async fn update_spent(
        connection: &mut SqliteConnection,
        spent_block_id: &str,
        outputs: &OutputChanges,
    ) -> Result<Vec<OutputKey>, IndexError> {
        let required = outputs.spent.iter().map(|key| (key, true));
        let tracked = outputs.tracked_spends.iter().map(|key| (key, false));
        let mut first = BTreeSet::new();
        let spends: Vec<_> = required
            .chain(tracked)
            .map(|(key, required)| (key, required, first.insert(Self::id_of(&key.output))))
            .collect();
        let distinct: Vec<String> = first.into_iter().collect();
        let unspent: BTreeSet<String> = Self::find_unspent(connection, &distinct)
            .await?
            .into_iter()
            .map(|row| row.id)
            .collect();
        Self::update_marked(connection, spent_block_id, &distinct).await?;
        let unknown: Vec<OutputKey> = spends
            .into_iter()
            .filter(|(key, required, first)| {
                *required && !(*first && unspent.contains(&Self::id_of(&key.output)))
            })
            .map(|(key, ..)| key.clone())
            .collect();
        // The rows carry no creation at all, which the table's own `CHECK`
        // permits for exactly this state and no other.
        let misses: Vec<_> = unknown
            .iter()
            .map(|key| Self::tombstone(key, spent_block_id))
            .collect();
        Self::insert_missed(connection, &misses).await?;
        Ok(unknown)
    }

    /// The unspent created rows among `ids`, many to a statement.
    async fn find_unspent(
        connection: &mut SqliteConnection,
        ids: &[String],
    ) -> Result<Vec<Self>, IndexError> {
        Chunks::new(ids, "failed to look up spent outputs")
            .fetch(connection, |chunk| {
                let mut statement = QueryBuilder::new(joined!(
                    "WHERE o.spent_block_id IS NULL AND o.asset IS NOT NULL AND o.id IN ("
                ));
                let mut bound = statement.separated(", ");
                for id in chunk {
                    bound.push_bind(id.as_str());
                }
                statement.push(")");
                statement
            })
            .await
    }

    /// Marks every unspent held output among `ids` spent in `spent_block_id`,
    /// many to a statement.
    async fn update_marked(
        connection: &mut SqliteConnection,
        spent_block_id: &str,
        ids: &[String],
    ) -> Result<(), IndexError> {
        Chunks::new(ids, "failed to spend outputs")
            .execute(connection, |chunk| {
                let mut statement = QueryBuilder::new("UPDATE output SET spent_block_id = ");
                statement
                    .push_bind(spent_block_id)
                    .push(" WHERE spent_block_id IS NULL AND id IN (");
                let mut bound = statement.separated(", ");
                for id in chunk {
                    bound.push_bind(id.as_str());
                }
                statement.push(")");
                statement
            })
            .await
            .map(drop)
    }

    /// Records every spend whose creation is not known yet, many to a
    /// statement; insert-or-ignore, because the same key may miss more than
    /// once when a chunk is redone.
    async fn insert_missed(
        connection: &mut SqliteConnection,
        rows: &[Self],
    ) -> Result<(), IndexError> {
        Chunks::new(rows, "failed to record unmatched spends")
            .execute(connection, |chunk| {
                let mut statement = QueryBuilder::new(
                    "INSERT OR IGNORE INTO output
                         (id, chain, network, transaction_id, output_index, address_id,
                          spent_block_id) ",
                );
                statement.push_values(chunk, |mut values, row: &Self| {
                    values
                        .push_bind(row.id.as_str())
                        .push_bind(row.network.chain.as_str())
                        .push_bind(row.network.name.as_str())
                        .push_bind(row.transaction_id.as_str())
                        .push_bind(row.output_index)
                        .push_bind(row.address_id.as_deref())
                        .push_bind(row.spent_block_id.as_deref());
                });
                statement
            })
            .await
            .map(drop)
    }

    /// One output's place, as an opaque cursor position: its id.
    pub(in crate::storage::sqlite) fn position(key: &OutputKey) -> Vec<u8> {
        Self::id_of(&key.output).into_bytes()
    }

    /// The output id one of this adapter's own cursor positions names.
    pub(in crate::storage::sqlite) fn resumed(position: &[u8]) -> Result<String, IndexError> {
        String::from_utf8(position.to_vec())
            .map_err(|_| store("an output cursor does not belong to this store"))
    }

    /// The row a creation in block `created_block_id` is written as, unspent.
    fn created(output: &IndexedOutput, created_block_id: &str) -> Self {
        Self {
            id: Self::id_of(&output.id),
            network: output.address.network.clone(),
            transaction_id: output.id.transaction.id.as_str().to_owned(),
            output_index: i64::from(output.id.index),
            address_id: Some(address_id(&output.address.network, &output.address.value)),
            creation: Some(Creation {
                asset: output.asset.asset.clone(),
                amount: output.amount.to_string(),
                evidence: output.evidence.clone(),
                created_block_id: created_block_id.to_owned(),
                coinbase: output.coinbase,
            }),
            created_height: None,
            spent_block_id: None,
        }
    }

    /// The row a required spend in `spent_block_id` leaves when the file holds
    /// no creation for the outpoint it spends: spent, and nothing else known.
    fn tombstone(key: &OutputKey, spent_block_id: &str) -> Self {
        Self {
            id: Self::id_of(&key.output),
            network: key.address.network.clone(),
            transaction_id: key.output.transaction.id.as_str().to_owned(),
            output_index: i64::from(key.output.index),
            address_id: Some(address_id(&key.address.network, &key.address.value)),
            creation: None,
            created_height: None,
            spent_block_id: Some(spent_block_id.to_owned()),
        }
    }
}

impl Creation {
    /// The creation `row` holds, or `None` when the row is a spend whose
    /// creation is not known yet.
    fn from_row(row: &SqliteRow) -> Result<Option<Self>, sqlx::Error> {
        let held = (
            column::<Option<String>>(row, "asset")?,
            column::<Option<String>>(row, "amount")?,
            column::<Option<Vec<u8>>>(row, "evidence")?,
            column::<Option<String>>(row, "created_block_id")?,
            column::<Option<i64>>(row, "coinbase")?,
        );
        match held {
            (Some(asset), Some(amount), Some(evidence), Some(created_block_id), Some(coinbase)) => {
                Ok(Some(Self {
                    asset,
                    amount,
                    evidence,
                    created_block_id,
                    coinbase: coinbase != 0,
                }))
            }
            (None, None, None, None, None) => Ok(None),
            _ => Err(refused("a stored output's creation is half written")),
        }
    }
}

impl TryFrom<Output> for IndexedOutput {
    type Error = IndexError;

    fn try_from(row: Output) -> Result<Self, IndexError> {
        let creation = row
            .creation
            .ok_or_else(|| store("a stored output has no creation"))?;
        let address = row
            .address_id
            .as_deref()
            .map(native_part)
            .ok_or_else(|| store("a stored live output has no address"))?;
        let height = row
            .created_height
            .ok_or_else(|| store("a stored output's creating block is missing"))?;
        Ok(Self {
            id: OutputId {
                transaction: TransactionRef {
                    network: row.network.clone(),
                    id: row.transaction_id.into(),
                },
                index: u32::try_from(row.output_index)
                    .map_err(|_| store("a stored output index is out of range"))?,
            },
            address: Address {
                network: row.network.clone(),
                value: address,
            },
            asset: AssetId {
                chain: row.network.chain,
                asset: creation.asset,
            },
            amount: amount(&creation.amount)?,
            evidence: creation.evidence,
            created_at: BlockHeight(restored(height)?),
            coinbase: creation.coinbase,
        })
    }
}
