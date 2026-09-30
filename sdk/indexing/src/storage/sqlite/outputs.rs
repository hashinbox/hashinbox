//! One outpoint's whole life: what created it, what spent it, and the page of
//! the ones still unspent.
//!
//! **Two tables became one, and the property they cooperated on became
//! structural.** A spend used to be `DELETE FROM live_output` plus, when it
//! matched nothing, a row in a second table that a later create had to remember
//! to consult. The row was the only thing standing between a backfill and a
//! permanently spendable phantom UTXO: the block that spent the output is above
//! the address's anchor and no chunk will ever carry it again, while the block
//! that created it is still below the checkpoint waiting to be filled in.
//!
//! Here the spend is an `UPDATE` that sets a flag and the create is an upsert
//! that fills the creation columns **only where they are null**. Filling the
//! creation of a row whose spend is already known leaves the flag set, so the
//! output is never offered again; and the redo of a batch whose outcome was
//! lost writes nothing new, because the row already has its creation. Neither
//! is a cancellation anybody has to perform.
//!
//! **Only required spends are remembered.** A tracked spend writes nothing, for
//! the same reason it reports nothing: no address claimed the outpoint, and the
//! backfill of the address that eventually does claim it anchors above this
//! block and so inspects this very spend again. A row per unwatched outpoint
//! spent in every block would grow with the chain, which is the one growth this
//! table must not have.

use rusqlite::{Connection, Row, Transaction, params};

use super::{Repository, Tables, conflict, failure, store};
use crate::{
    BlockHeight, BoxFuture, IndexError, IndexedOutput, Inspection, OutputChanges, OutputCursor,
    OutputId, OutputKey, OutputPage, OutputRequest, OutputStore, storage::Probe,
};

/// The creation columns every read selects, in the mapping's order. A row whose
/// spend is known before its creation holds `NULL` in all of them but the two
/// that address it, which is why the unspent page never returns one.
const COLUMNS: &str =
    "transaction_id, output_index, asset, amount, evidence, created_height,      coinbase";

/// The two columns that address one output: `(transaction id, output index)`,
/// which is the order `output` is stored and resumed in.
type Outpoint = (String, i64);

impl OutputStore for Repository {
    fn list<'a>(&'a self, request: OutputRequest) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
        Box::pin(async move {
            request.validate()?;
            let tables = self.tables(&request.network)?;
            self.database
                .blocking(move |connection| tables.outputs(connection, &request))
                .await
        })
    }
}

impl Tables {
    /// One checkpoint-consistent page of live outputs.
    pub(super) fn outputs(
        &self,
        connection: &Connection,
        request: &OutputRequest,
    ) -> Result<OutputPage, IndexError> {
        let (read, checkpoint) = self.snapshot(connection)?;
        // Refused from this snapshot rather than by whoever reads the page: the
        // observations and the rows are one transaction, so the gate and the
        // rows it guards cannot be read at two different lines.
        let observations = self.observing(&read, &request.address.value)?;
        Inspection::new(&observations, checkpoint.as_ref()).spendable()?;
        let after = match &request.after {
            // The live set is not append-only -- a spend removes a row -- so a
            // cursor is still bound to the checkpoint it was issued at.
            Some(cursor) if cursor.checkpoint != checkpoint => {
                return Err(conflict("outputs changed during pagination"));
            }
            Some(cursor) => Self::resumed(&cursor.position)?,
            None => (String::new(), -1),
        };
        let probe = Probe::new(request.limit);
        let live = self.live(&read, &request.address.value, &after, probe.rows())?;
        let (outputs, next) = probe.page(live, |output| OutputCursor {
            checkpoint: checkpoint.clone(),
            position: Self::position(&output.key()),
            // Nothing here folds a pending block, so there is no revision
            // to name: whoever folds one sets it on the way out.
            revision: None,
        });
        Ok(OutputPage {
            checkpoint,
            outputs,
            next,
        })
    }

    /// Up to `limit` of `address`'s live outputs after `after`, in stored
    /// order.
    fn live(
        &self,
        read: &Transaction<'_>,
        address: &str,
        after: &Outpoint,
        limit: usize,
    ) -> Result<Vec<IndexedOutput>, IndexError> {
        let mut statement = read
            .prepare_cached(&format!(
                "SELECT {COLUMNS} FROM output
                 WHERE chain = ?1 AND network = ?2 AND address = ?3 AND spent = 0
                   AND (transaction_id, output_index) > (?4, ?5)
                 ORDER BY transaction_id, output_index LIMIT ?6"
            ))
            .map_err(|error| failure(&error, "failed to prepare an output read"))?;
        let bound = i64::try_from(limit).unwrap_or(i64::MAX);
        let arguments = params![self.chain(), self.name(), address, after.0, after.1, bound];
        let mut rows = statement
            .query(arguments)
            .map_err(|error| failure(&error, "failed to read live outputs"))?;
        let mut outputs = Vec::new();
        while let Some(row) = Self::step(&mut rows)? {
            outputs.push(self.output(row, address)?);
        }
        Ok(outputs)
    }

    /// Exactly the creation `key` names, spent or not, or `None` when the file
    /// holds no creation for it.
    ///
    /// Unlike the page, this does **not** narrow to the unspent set: the row it
    /// answers for is the one a re-presented creation is compared against, and a
    /// creation filled into a row whose spend was already known is still that
    /// row. Narrowing here would make the redo of a lost batch read as a
    /// disagreement.
    fn held(
        &self,
        read: &Transaction<'_>,
        key: &OutputKey,
    ) -> Result<Option<IndexedOutput>, IndexError> {
        let (transaction, index) = Self::identity(key);
        let mut statement = read
            .prepare_cached(&format!(
                "SELECT {COLUMNS} FROM output
                 WHERE chain = ?1 AND network = ?2 AND address = ?3
                   AND transaction_id = ?4 AND output_index = ?5
                   AND asset IS NOT NULL"
            ))
            .map_err(|error| failure(&error, "failed to prepare an output lookup"))?;
        let arguments = params![
            self.chain(),
            self.name(),
            key.address.value,
            transaction,
            index
        ];
        let mut rows = statement
            .query(arguments)
            .map_err(|error| failure(&error, "failed to look up a live output"))?;
        match Self::step(&mut rows)? {
            Some(row) => self.output(row, &key.address.value).map(Some),
            None => Ok(None),
        }
    }

    /// One `output` row's creation half as the output it holds.
    fn output(&self, row: &Row<'_>, address: &str) -> Result<IndexedOutput, IndexError> {
        let index: i64 = Self::column(row, 1)?;
        Ok(IndexedOutput {
            id: OutputId {
                transaction: self.transaction(Self::column(row, 0)?),
                index: u32::try_from(index)
                    .map_err(|_| store("a stored output index is out of range"))?,
            },
            address: self.address(address.to_owned()),
            asset: self.asset(Self::column(row, 2)?),
            amount: Self::decimal(row, 3)?,
            evidence: Self::column(row, 4)?,
            created_at: BlockHeight(Self::restored(Self::column(row, 5)?)?),
            coinbase: Self::column::<i64>(row, 6)? != 0,
        })
    }

    /// Writes the batch's surviving outputs, verifying any the index already
    /// holds rather than refusing them: a redone batch re-presents its own, and
    /// one `(address, outpoint)` cannot honestly have two contents.
    ///
    /// One rule for both directions, and it is no weaker than the strict insert
    /// it replaced: a block extending the line carries outputs above every block
    /// the file holds, so it cannot collide with a row it did not write, and a
    /// chain that spent an outpoint cannot create it afterwards.
    ///
    /// An output some earlier spend already named keeps that spend. The upsert
    /// fills the creation columns only where they are null, so discovering a
    /// creation below the checkpoint can never erase a spending above it — the
    /// property the two-table shape had to cooperate to hold, and the reason the
    /// redo of a lost batch is a no-op rather than a second create.
    pub(super) fn fill_outputs(
        &self,
        transaction: &Transaction<'_>,
        outputs: &[IndexedOutput],
    ) -> Result<(), IndexError> {
        for output in outputs {
            if self.insert_output(transaction, output)? {
                continue;
            }
            self.verify_output(transaction, output)?;
        }
        Ok(())
    }

    /// One upsert; `false` when the row already carried a creation.
    ///
    /// `DO UPDATE ... WHERE output.asset IS NULL` is the whole of it: an
    /// existing row with no creation is a spend that arrived first and is filled
    /// in here, and an existing row that already has one is left exactly as it
    /// is for [`Self::verify_output`] to compare.
    fn insert_output(
        &self,
        transaction: &Transaction<'_>,
        output: &IndexedOutput,
    ) -> Result<bool, IndexError> {
        let mut statement = transaction
            .prepare_cached(
                "INSERT INTO output
                     (chain, network, address, transaction_id, output_index,
                      asset, amount, evidence, created_height, coinbase, spent)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0)
                 ON CONFLICT (chain, network, address, transaction_id, output_index)
                 DO UPDATE SET asset = excluded.asset, amount = excluded.amount,
                     evidence = excluded.evidence,
                     created_height = excluded.created_height,
                     coinbase = excluded.coinbase
                 WHERE output.asset IS NULL",
            )
            .map_err(|error| failure(&error, "failed to prepare an output write"))?;
        let written = statement
            .execute(params![
                self.chain(),
                self.name(),
                output.address.value,
                output.id.transaction.id.as_str(),
                i64::from(output.id.index),
                output.asset.asset,
                output.amount.to_string(),
                output.evidence,
                Self::stored(output.created_at.0)?,
                i64::from(output.coinbase),
            ])
            .map_err(|error| failure(&error, "failed to write an output"))?;
        Ok(written > 0)
    }

    /// Checks that the creation the file already holds is the one being
    /// written.
    fn verify_output(
        &self,
        transaction: &Transaction<'_>,
        output: &IndexedOutput,
    ) -> Result<(), IndexError> {
        if self.held(transaction, &output.key())?.as_ref() == Some(output) {
            return Ok(());
        }
        Err(store("a backfilled output disagrees with the stored one"))
    }

    /// Marks every spent output the index holds as spent, recording and
    /// reporting the required spends whose outputs it does not hold.
    ///
    /// A required spend of an output this network never indexed is the normal
    /// outcome of watching an already-funded address, so it is reported rather
    /// than refused (`docs/design/024` section 5.1). Nothing is recorded to put
    /// an output back: a confirmed output is only ever spent by a confirmed
    /// block, and no confirmed block is ever undone (`docs/design/030 §8
    /// (ex-025)`).
    ///
    /// What *is* recorded is the spend itself, against an outpoint whose
    /// creation is still unknown, because that creation may arrive from below.
    /// A tracked spend records nothing, for the same reason it reports nothing —
    /// no address claimed the outpoint, and the backfill of the address that
    /// eventually does claim it anchors above this block and so inspects this
    /// very spend again. Writing one would be a row per unwatched outpoint per
    /// block.
    pub(super) fn spend(
        &self,
        transaction: &Transaction<'_>,
        outputs: &OutputChanges,
    ) -> Result<Vec<OutputKey>, IndexError> {
        let required = outputs.spent.iter().map(|key| (key, true));
        let tracked = outputs.tracked_spends.iter().map(|key| (key, false));
        let mut unknown = Vec::new();
        for (key, required) in required.chain(tracked) {
            if self.mark(transaction, key)? || !required {
                continue;
            }
            self.record_miss(transaction, key)?;
            unknown.push(key.clone());
        }
        Ok(unknown)
    }

    /// Marks one held output spent; `false` when the index holds none.
    fn mark(&self, transaction: &Transaction<'_>, key: &OutputKey) -> Result<bool, IndexError> {
        let (identity, index) = Self::identity(key);
        let marked = transaction
            .prepare_cached(
                "UPDATE output SET spent = 1
                 WHERE chain = ?1 AND network = ?2 AND address = ?3
                   AND transaction_id = ?4 AND output_index = ?5 AND spent = 0",
            )
            .and_then(|mut statement| {
                statement.execute(params![
                    self.chain(),
                    self.name(),
                    key.address.value,
                    identity,
                    index
                ])
            })
            .map_err(|error| failure(&error, "failed to spend an output"))?;
        Ok(marked > 0)
    }

    /// Records that one required spend matched no known creation.
    ///
    /// Insert-or-ignore, because the same key may miss more than once: a chunk
    /// redone after a lost outcome presents its spends again, and each of them
    /// has one row or none. The row carries no creation at all, which the
    /// table's own `CHECK` permits for exactly this state and no other.
    fn record_miss(
        &self,
        transaction: &Transaction<'_>,
        key: &OutputKey,
    ) -> Result<(), IndexError> {
        let (identity, index) = Self::identity(key);
        transaction
            .prepare_cached(
                "INSERT OR IGNORE INTO output
                     (chain, network, address, transaction_id, output_index, spent)
                 VALUES (?1, ?2, ?3, ?4, ?5, 1)",
            )
            .and_then(|mut statement| {
                statement.execute(params![
                    self.chain(),
                    self.name(),
                    key.address.value,
                    identity,
                    index
                ])
            })
            .map(drop)
            .map_err(|error| failure(&error, "failed to record an unmatched spend"))
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
    fn position(key: &OutputKey) -> Vec<u8> {
        let identity = key.output.transaction.id.as_str().as_bytes();
        let mut position = (identity.len() as u64).to_be_bytes().to_vec();
        position.extend_from_slice(identity);
        position.extend_from_slice(&key.output.index.to_be_bytes());
        position
    }

    /// The place one of this adapter's own cursor positions names.
    fn resumed(position: &[u8]) -> Result<Outpoint, IndexError> {
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
}
