//! `output`: one outpoint's whole life — what created it, what spent it, and
//! the page of the ones still unspent.
//!
//! **One table, and the property two tables cooperated on is structural.** A
//! spend is an `UPDATE` that sets a flag and a create is an upsert that fills
//! the creation columns **only where they are null**. Filling the creation of a
//! row whose spend is already known leaves the flag set, so the output is never
//! offered again — the block that spent it is above the address's anchor and no
//! chunk will carry it again, while the block that created it may still be
//! below the checkpoint waiting to be filled in. The redo of a batch whose
//! outcome was lost writes nothing new, because the row already has its
//! creation. Neither is a cancellation anybody has to perform.
//!
//! **Only required spends are remembered.** A tracked spend writes nothing, for
//! the same reason it reports nothing: no address claimed the outpoint, and the
//! backfill of the address that eventually does claim it anchors above this
//! block and so inspects this very spend again. A row per unwatched outpoint
//! spent in every block would grow with the chain, which is the one growth this
//! table must not have.

use std::collections::{BTreeMap, BTreeSet};

use sqlx::{FromRow, QueryBuilder, Sqlite, SqliteConnection, sqlite::SqliteRow};

use super::{
    Chunks, amount, column, deleted, fetched, framed, network, refused, restored, store, stored,
};
use crate::{
    Address, AssetId, BlockHeight, IndexError, IndexedOutput, Network, OutputChanges, OutputId,
    OutputKey, TransactionRef,
};

/// The two columns that address one output: `(transaction id, output index)`,
/// which is the order `output` is stored and resumed in.
pub(in crate::storage::sqlite) type Outpoint = (String, i64);

/// What created an output: the five columns a creation fills together.
#[derive(Debug, PartialEq, Eq)]
struct Creation {
    asset: String,
    amount: String,
    evidence: Vec<u8>,
    created_height: i64,
    coinbase: bool,
}

/// One `output` row.
///
/// A row whose spend arrived before its creation has no creation yet: the
/// spend is remembered so that filling the creation in later can never offer
/// the output again.
#[derive(Debug)]
pub(in crate::storage::sqlite) struct Output {
    network: Network,
    address: String,
    transaction_id: String,
    output_index: i64,
    creation: Option<Creation>,
    spent: bool,
}

impl FromRow<'_, SqliteRow> for Output {
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            network: network(row)?,
            address: column(row, "address")?,
            transaction_id: column(row, "transaction_id")?,
            output_index: column(row, "output_index")?,
            creation: Creation::from_row(row)?,
            spent: column::<i64>(row, "spent")? != 0,
        })
    }
}

impl Output {
    /// Whether the row is still spendable: created and not spent.
    const fn live(&self) -> bool {
        self.creation.is_some() && !self.spent
    }

    /// Whether `other` records the same creation as this row.
    ///
    /// The comparison a re-presented creation is verified by: the spend flag is
    /// not part of it, because a creation filled into a row whose spend was
    /// already known is still that row.
    fn same_creation(&self, other: &Self) -> bool {
        self.creation == other.creation
    }

    /// What one row is the only row for: its primary key.
    fn key(&self) -> (&Network, &str, &str, i64) {
        (
            &self.network,
            &self.address,
            &self.transaction_id,
            self.output_index,
        )
    }

    /// Up to `limit` of `address`'s live outputs after `after`, in stored
    /// order: created, and not spent.
    pub(in crate::storage::sqlite) async fn find_live(
        connection: &mut SqliteConnection,
        network: &Network,
        address: &str,
        after: &Outpoint,
        limit: usize,
    ) -> Result<Vec<Self>, IndexError> {
        let bound = i64::try_from(limit).unwrap_or(i64::MAX);
        let query = framed(Self::LIVE, network)
            .bind(address)
            .bind(&after.0)
            .bind(after.1)
            .bind(bound);
        let rows: Vec<Self> = fetched(connection, query, "failed to read live outputs").await?;
        if rows.iter().any(|row| !row.live()) {
            return Err(store("a stored live output is not spendable"));
        }
        Ok(rows)
    }

    /// The statement [`Self::find_live`] runs.
    pub(in crate::storage::sqlite) const LIVE: &str = "SELECT * FROM output
         WHERE chain = ?1 AND network = ?2 AND address = ?3 AND spent = 0
           AND (transaction_id, output_index) > (?4, ?5)
         ORDER BY transaction_id, output_index LIMIT ?6";

    /// Writes the batch's surviving outputs, verifying any the index already
    /// holds rather than refusing them: a redone batch re-presents its own, and
    /// one `(address, outpoint)` cannot honestly have two contents.
    ///
    /// One rule for both directions, and it is no weaker than a strict insert:
    /// a block extending the line carries outputs above every block the file
    /// holds, so it cannot collide with a row it did not write, and a chain
    /// that spent an outpoint cannot create it afterwards.
    ///
    /// An output some earlier spend already named keeps that spend. The upsert
    /// fills the creation columns only where they are null, so discovering a
    /// creation below the checkpoint can never erase a spending above it, and
    /// the redo of a lost batch is a no-op rather than a second create.
    pub(in crate::storage::sqlite) async fn insert_created(
        connection: &mut SqliteConnection,
        outputs: &[IndexedOutput],
    ) -> Result<(), IndexError> {
        let rows = outputs
            .iter()
            .map(Self::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let written = Self::upsert_many(connection, &rows).await?;
        if usize::try_from(written).ok() == Some(rows.len()) {
            return Ok(());
        }
        // Some row was neither written nor filled in: compare every row with
        // the creation the file holds, which for a row this very statement
        // wrote is its own.
        let held = Self::find_created(connection, &rows).await?;
        let held: BTreeMap<_, _> = held.iter().map(|row| (row.key(), row)).collect();
        let agrees = |row: &Self| {
            held.get(&row.key())
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
    ///
    /// `DO UPDATE ... WHERE output.asset IS NULL` is the whole of it: an
    /// existing row with no creation is a spend that arrived first and is
    /// filled in here, keeping its spend, and an existing row that already has
    /// one is left exactly as it is for the caller to compare.
    async fn upsert_many(
        connection: &mut SqliteConnection,
        rows: &[Self],
    ) -> Result<u64, IndexError> {
        Chunks::new(rows, "failed to write outputs")
            .execute(connection, |chunk| {
                let mut statement = QueryBuilder::new(
                    "INSERT INTO output
                         (chain, network, address, transaction_id, output_index,
                          asset, amount, evidence, created_height, coinbase, spent) ",
                );
                statement.push_values(chunk, |mut values, row: &Self| {
                    let creation = row.creation.as_ref();
                    values
                        .push_bind(row.network.chain.as_str())
                        .push_bind(row.network.name.as_str())
                        .push_bind(row.address.as_str())
                        .push_bind(row.transaction_id.as_str())
                        .push_bind(row.output_index)
                        .push_bind(creation.map(|creation| creation.asset.as_str()))
                        .push_bind(creation.map(|creation| creation.amount.as_str()))
                        .push_bind(creation.map(|creation| creation.evidence.as_slice()))
                        .push_bind(creation.map(|creation| creation.created_height))
                        .push_bind(creation.map(|creation| i64::from(creation.coinbase)))
                        .push_bind(i64::from(row.spent));
                });
                statement.push(
                    " ON CONFLICT (chain, network, address, transaction_id, output_index)
                     DO UPDATE SET asset = excluded.asset, amount = excluded.amount,
                         evidence = excluded.evidence,
                         created_height = excluded.created_height,
                         coinbase = excluded.coinbase
                     WHERE output.asset IS NULL",
                );
                statement
            })
            .await
    }

    /// The created rows the file holds under the keys of `rows`, spent or
    /// not, many keys to a statement, in no particular order; a key the file
    /// holds no creation for has none.
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
                let mut statement = QueryBuilder::new(
                    "SELECT * FROM output
                     WHERE asset IS NOT NULL
                       AND (chain, network, address, transaction_id, output_index) IN (",
                );
                statement.push_values(chunk, |mut key, row: &Self| {
                    key.push_bind(row.network.chain.as_str())
                        .push_bind(row.network.name.as_str())
                        .push_bind(row.address.as_str())
                        .push_bind(row.transaction_id.as_str())
                        .push_bind(row.output_index);
                });
                statement.push(")");
                statement
            })
            .await
    }

    /// Marks every spent output the index holds as spent, recording and
    /// reporting the required spends whose outputs it does not hold.
    ///
    /// A required spend of an output this network never indexed is the normal
    /// outcome of watching an already-funded address, so it is reported rather
    /// than refused. Nothing is recorded to put an output back: a confirmed
    /// output is only ever spent by a confirmed block, and no confirmed block
    /// is ever undone.
    ///
    /// What *is* recorded is the spend itself, against an outpoint whose
    /// creation is still unknown, because that creation may arrive from below.
    /// A tracked spend records nothing, for the same reason it reports nothing —
    /// no address claimed the outpoint, and the backfill of the address that
    /// eventually does claim it anchors above this block and so inspects this
    /// very spend again. Writing one would be a row per unwatched outpoint per
    /// block.
    ///
    /// Spends are taken in order, required before tracked, and only the first
    /// spend of an outpoint can find it unspent: a repeat finds it spent, as it
    /// would have one statement at a time.
    pub(in crate::storage::sqlite) async fn update_spent(
        connection: &mut SqliteConnection,
        network: &Network,
        outputs: &OutputChanges,
    ) -> Result<Vec<OutputKey>, IndexError> {
        let required = outputs.spent.iter().map(|key| (key, true));
        let tracked = outputs.tracked_spends.iter().map(|key| (key, false));
        let mut first = BTreeSet::new();
        let spends: Vec<_> = required
            .chain(tracked)
            .map(|(key, required)| (key, required, first.insert(key)))
            .collect();
        let distinct: Vec<&OutputKey> = first.into_iter().collect();
        let unspent: BTreeSet<_> = Self::find_unspent(connection, network, &distinct)
            .await?
            .into_iter()
            .map(|row| (row.address, row.transaction_id, row.output_index))
            .collect();
        Self::update_marked(connection, network, &distinct).await?;
        let unknown: Vec<OutputKey> = spends
            .into_iter()
            .filter(|(key, required, first)| {
                *required && !(*first && unspent.contains(&Self::outpoint(key)))
            })
            .map(|(key, ..)| key.clone())
            .collect();
        // The rows carry no creation at all, which the table's own `CHECK`
        // permits for exactly this state and no other.
        let misses: Vec<_> = unknown.iter().map(Self::from).collect();
        Self::insert_missed(connection, &misses).await?;
        Ok(unknown)
    }

    /// The unspent rows of `network` among `keys`, many keys to a statement.
    async fn find_unspent(
        connection: &mut SqliteConnection,
        network: &Network,
        keys: &[&OutputKey],
    ) -> Result<Vec<Self>, IndexError> {
        Chunks::new(keys, "failed to look up spent outputs")
            .fetch(connection, |chunk| {
                Self::outpoints("SELECT * FROM output", network, chunk)
            })
            .await
    }

    /// Marks every held output of `keys` spent, many keys to a statement.
    async fn update_marked(
        connection: &mut SqliteConnection,
        network: &Network,
        keys: &[&OutputKey],
    ) -> Result<(), IndexError> {
        Chunks::new(keys, "failed to spend outputs")
            .execute(connection, |chunk| {
                Self::outpoints("UPDATE output SET spent = 1", network, chunk)
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
                         (chain, network, address, transaction_id, output_index, spent) ",
                );
                statement.push_values(chunk, |mut values, row: &Self| {
                    values
                        .push_bind(row.network.chain.as_str())
                        .push_bind(row.network.name.as_str())
                        .push_bind(row.address.as_str())
                        .push_bind(row.transaction_id.as_str())
                        .push_bind(row.output_index)
                        .push_bind(i64::from(row.spent));
                });
                statement
            })
            .await
            .map(drop)
    }

    /// `head` narrowed to the unspent rows of `network` among `keys`.
    fn outpoints<'q>(
        head: &'static str,
        network: &'q Network,
        keys: &'q [&'q OutputKey],
    ) -> QueryBuilder<'q, Sqlite> {
        let mut statement = QueryBuilder::new(head);
        statement
            .push(" WHERE chain = ")
            .push_bind(network.chain.as_str())
            .push(" AND network = ")
            .push_bind(network.name.as_str())
            .push(" AND spent = 0 AND (address, transaction_id, output_index) IN (");
        statement.push_values(keys, |mut outpoint, key| {
            let (identity, index) = Self::identity(key);
            outpoint
                .push_bind(key.address.value.as_str())
                .push_bind(identity)
                .push_bind(index);
        });
        statement.push(")");
        statement
    }

    /// One output's `(address, transaction id, output index)` columns, owned.
    fn outpoint(key: &OutputKey) -> (String, String, i64) {
        let (identity, index) = Self::identity(key);
        (key.address.value.clone(), identity.to_owned(), index)
    }

    /// The two columns that address one output.
    fn identity(key: &OutputKey) -> (&str, i64) {
        (
            key.output.transaction.id.as_str(),
            i64::from(key.output.index),
        )
    }

    /// One output's place, as an opaque cursor position.
    ///
    /// Length-framed so the two halves cannot be confused, and the index is
    /// stored apart rather than appended to the identifier, because the order
    /// this resumes is the table's `(transaction_id, output_index)` and not a
    /// byte comparison of the pair.
    pub(in crate::storage::sqlite) fn position(key: &OutputKey) -> Vec<u8> {
        let identity = key.output.transaction.id.as_str().as_bytes();
        let mut position = (identity.len() as u64).to_be_bytes().to_vec();
        position.extend_from_slice(identity);
        position.extend_from_slice(&key.output.index.to_be_bytes());
        position
    }

    /// The place one of this adapter's own cursor positions names.
    pub(in crate::storage::sqlite) fn resumed(position: &[u8]) -> Result<Outpoint, IndexError> {
        let invalid = || store("an output cursor does not belong to this store");
        let (framed, rest) = position.split_at_checked(8).ok_or_else(invalid)?;
        let length = usize::try_from(u64::from_be_bytes(
            framed.try_into().map_err(|_| invalid())?,
        ))
        .map_err(|_| invalid())?;
        let (identity, index) = rest.split_at_checked(length).ok_or_else(invalid)?;
        let index: [u8; 4] = index.try_into().map_err(|_| invalid())?;
        let identity = String::from_utf8(identity.to_vec()).map_err(|_| invalid())?;
        Ok((identity, i64::from(u32::from_be_bytes(index))))
    }

    /// Removes every output of `network`, spent or not; how many rows that
    /// was.
    pub(in crate::storage::sqlite) async fn purge(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> Result<u64, IndexError> {
        deleted(
            connection,
            network,
            "DELETE FROM output WHERE chain = ?1 AND network = ?2",
        )
        .await
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
            column::<Option<i64>>(row, "created_height")?,
            column::<Option<i64>>(row, "coinbase")?,
        );
        match held {
            (Some(asset), Some(amount), Some(evidence), Some(created_height), Some(coinbase)) => {
                Ok(Some(Self {
                    asset,
                    amount,
                    evidence,
                    created_height,
                    coinbase: coinbase != 0,
                }))
            }
            (None, None, None, None, None) => Ok(None),
            _ => Err(refused("a stored output's creation is half written")),
        }
    }
}

impl From<&OutputKey> for Output {
    /// The row a required spend leaves when the file holds no creation for the
    /// outpoint it spends: spent, and nothing else known.
    fn from(key: &OutputKey) -> Self {
        Self {
            network: key.address.network.clone(),
            address: key.address.value.clone(),
            transaction_id: key.output.transaction.id.as_str().to_owned(),
            output_index: i64::from(key.output.index),
            creation: None,
            spent: true,
        }
    }
}

impl TryFrom<&IndexedOutput> for Output {
    type Error = IndexError;

    /// The row a creation is written as, unspent.
    fn try_from(output: &IndexedOutput) -> Result<Self, IndexError> {
        Ok(Self {
            network: output.address.network.clone(),
            address: output.address.value.clone(),
            transaction_id: output.id.transaction.id.as_str().to_owned(),
            output_index: i64::from(output.id.index),
            creation: Some(Creation {
                asset: output.asset.asset.clone(),
                amount: output.amount.to_string(),
                evidence: output.evidence.clone(),
                created_height: stored(output.created_at.0)?,
                coinbase: output.coinbase,
            }),
            spent: false,
        })
    }
}

impl TryFrom<Output> for IndexedOutput {
    type Error = IndexError;

    fn try_from(row: Output) -> Result<Self, IndexError> {
        let creation = row
            .creation
            .ok_or_else(|| store("a stored output has no creation"))?;
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
                value: row.address,
            },
            asset: AssetId {
                chain: row.network.chain,
                asset: creation.asset,
            },
            amount: amount(&creation.amount)?,
            evidence: creation.evidence,
            created_at: BlockHeight(restored(creation.created_height)?),
            coinbase: creation.coinbase,
        })
    }
}
