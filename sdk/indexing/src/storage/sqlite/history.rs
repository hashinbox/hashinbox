//! Confirmed transaction pages from address membership and shared transaction facts.
//! Native transaction order makes the cursor independent of which addresses were
//! selected when facts first arrived; the snapshot also carries watch coverage.

use rusqlite::{Connection, Row, Transaction, params};

use super::{Repository, Tables, failure, store, tables::Place};
use crate::{
    Address, BoxFuture, CanonicalPage, CanonicalStatus, CanonicalTransaction, ConfirmedHistory,
    HistoryPosition, HistoryQuery, Holder, IndexError, NetworkFee, storage::Probe,
};

/// Fewest rows one query reads while filling a page: a page of a busy address
/// in a rare asset reads many rows, so a small `limit` must not mean one query
/// per row.
const MIN_CHUNK: usize = 8;

/// The transaction columns every read selects, in the order the mapping
/// expects.
const COLUMNS: &str = "t.block_height, t.transaction_id, t.execution_status, t.failure_reason, \
     t.block_position, t.block_hash, t.block_parent_position, t.block_parent_hash, \
     t.block_timestamp, t.fee_asset, t.fee_amount, t.fee_payer, t.transaction_order";

/// The join every read walks: one address's memberships, each beside the
/// transaction it names.
const FROM: &str = "FROM transaction_address a
                    JOIN transaction_fact t
                      ON t.chain = a.chain AND t.network = a.network
                     AND t.transaction_id = a.transaction_id";

impl ConfirmedHistory for Repository {
    /// Reads the checkpoint and the page inside one deferred transaction, so
    /// the page is one snapshot rather than several reads that agreed
    /// afterwards.
    fn history<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> BoxFuture<'a, Result<CanonicalPage, IndexError>> {
        Box::pin(async move {
            request.validate()?;
            let tables = self.tables(&request.address.network)?;
            self.database
                .blocking(move |connection| tables.page(connection, &request))
                .await
        })
    }
}

impl Tables {
    /// One checkpoint-consistent page of `request`'s history.
    ///
    /// The page's provenance is read inside the same snapshot as its rows, and
    /// by the same method the balance fold asks ([`Tables::provenance`]): what
    /// the store can prove about the range behind an address's rows is one
    /// fact, so the two reads cannot answer it differently.
    pub(super) fn page(
        &self,
        connection: &Connection,
        request: &HistoryQuery,
    ) -> Result<CanonicalPage, IndexError> {
        let (read, checkpoint) = self.snapshot(connection)?;
        let completeness = self.provenance(&read, request, checkpoint.as_ref())?;
        let probe = Probe::new(request.limit);
        let matches = self.matching(&read, request, &probe)?;
        let (transactions, next) = probe.page(matches, |last| last.into());
        Ok(CanonicalPage {
            checkpoint,
            transactions,
            next,
            completeness,
        })
    }

    /// The transactions after the position that match `request`, in history
    /// order and at least as many as `probe` asks for when that many exist,
    /// each as [`HistoryQuery::select`] returns it.
    fn matching(
        &self,
        read: &Transaction<'_>,
        request: &HistoryQuery,
        probe: &Probe,
    ) -> Result<Vec<CanonicalTransaction>, IndexError> {
        let mut after = Self::anchored(request.after.as_ref())?;
        let chunk = probe.rows().max(MIN_CHUNK);
        let mut matches = Vec::new();
        loop {
            let entries = self.entries(read, &request.address.value, None, &after, chunk)?;
            let exhausted = entries.len() < chunk;
            for (place, entry) in entries {
                after = place;
                matches.extend(request.select(entry));
            }
            if exhausted || probe.answered(matches.len()) {
                return Ok(matches);
            }
        }
    }

    /// Reads rows in native block order. Cursor order never depends on the subset
    /// of transactions selected for one address.
    fn anchored(after: Option<&HistoryPosition>) -> Result<Place, IndexError> {
        let Some(after) = after else {
            return Ok(Place {
                height: -1,
                order: -1,
                id: String::new(),
            });
        };
        Ok(Place {
            height: Self::stored(after.height.0)?,
            order: Self::stored(after.transaction_order)?,
            id: after.transaction.id.as_str().to_owned(),
        })
    }

    /// One chunk of `address`'s history after `after`, movements attached.
    ///
    /// `asset` narrows what is attached without narrowing what is read: a
    /// membership counts for a holding even when none of its movements are in
    /// that asset, because the transaction may still carry a fee the holding
    /// paid.
    pub(super) fn entries(
        &self,
        read: &Transaction<'_>,
        address: &str,
        asset: Option<&str>,
        after: &Place,
        limit: usize,
    ) -> Result<Vec<(Place, CanonicalTransaction)>, IndexError> {
        let mut statement = read
            .prepare_cached(&format!(
                "SELECT {COLUMNS} {FROM}
                 WHERE a.chain = ?1 AND a.network = ?2 AND a.address = ?3
                   AND (a.block_height, a.transaction_order, a.transaction_id) > (?4, ?5, ?6)
                 ORDER BY a.block_height, a.transaction_order, a.transaction_id LIMIT ?7"
            ))
            .map_err(|error| failure(&error, "failed to prepare a history read"))?;
        let bound = i64::try_from(limit).unwrap_or(i64::MAX);
        let arguments = params![
            self.chain(),
            self.name(),
            address,
            after.height,
            after.order,
            after.id,
            bound
        ];
        let mut rows = statement
            .query(arguments)
            .map_err(|error| failure(&error, "failed to read history"))?;
        let mut entries = Vec::new();
        while let Some(row) = Self::step(&mut rows)? {
            entries.push(self.assemble(row)?);
        }
        self.attach(read, address, asset, entries)
    }

    /// Exactly the transaction `identity` names, without its movements, or
    /// `None`.
    pub(super) fn fact(
        &self,
        read: &Transaction<'_>,
        identity: &str,
    ) -> Result<Option<(Place, CanonicalTransaction)>, IndexError> {
        let mut statement = read
            .prepare_cached(&format!(
                "SELECT {COLUMNS} FROM transaction_fact t
                 WHERE t.chain = ?1 AND t.network = ?2 AND t.transaction_id = ?3"
            ))
            .map_err(|error| failure(&error, "failed to prepare a transaction lookup"))?;
        let arguments = params![self.chain(), self.name(), identity];
        let mut rows = statement
            .query(arguments)
            .map_err(|error| failure(&error, "failed to look up a transaction"))?;
        match Self::step(&mut rows)? {
            Some(row) => self.assemble(row).map(Some),
            None => Ok(None),
        }
    }

    /// Fetches every movement of `entries` in one statement, bounded by the
    /// first and last entry the chunk holds, and gives each entry its own.
    fn attach(
        &self,
        read: &Transaction<'_>,
        address: &str,
        asset: Option<&str>,
        mut entries: Vec<(Place, CanonicalTransaction)>,
    ) -> Result<Vec<(Place, CanonicalTransaction)>, IndexError> {
        let (Some((lower, _)), Some((upper, _))) = (entries.first(), entries.last()) else {
            return Ok(entries);
        };
        let mut held = self.attached(read, address, asset, (lower, upper))?;
        for (place, entry) in &mut entries {
            entry.movements = held.remove(&place.id).unwrap_or_default();
        }
        Ok(entries)
    }

    /// One `transaction_fact` row as the transaction it holds, before its
    /// movements are attached, and the place it sits at.
    fn assemble(&self, row: &Row<'_>) -> Result<(Place, CanonicalTransaction), IndexError> {
        let block = Self::block((
            Self::column(row, 4)?,
            Self::column(row, 0)?,
            Self::column(row, 5)?,
            Self::column(row, 6)?,
            Self::column(row, 7)?,
            Self::column(row, 8)?,
        ))?;
        let failed: i64 = Self::column(row, 2)?;
        let reason: Option<String> = Self::column(row, 3)?;
        let identity: String = Self::column(row, 1)?;
        let order: i64 = Self::column(row, 12)?;
        let place = Place {
            height: Self::column(row, 0)?,
            order,
            id: identity.clone(),
        };
        Ok((
            place,
            CanonicalTransaction {
                transaction_id: self.transaction(identity),
                transaction_order: Self::restored(order)?,
                status: match failed {
                    0 => CanonicalStatus::Included { block },
                    _ => CanonicalStatus::Failed { block, reason },
                },
                movements: Vec::new(),
                fee: self.fee(row)?,
            },
        ))
    }

    /// The three fee columns of one row as the fee they hold.
    fn fee(&self, row: &Row<'_>) -> Result<Option<NetworkFee>, IndexError> {
        let asset: Option<String> = Self::column(row, 9)?;
        let amount: Option<String> = Self::column(row, 10)?;
        let payer: Option<String> = Self::column(row, 11)?;
        match (asset, amount) {
            (Some(asset), Some(amount)) => Ok(Some(NetworkFee {
                asset: self.asset(asset),
                amount: Self::amount(&amount)?,
                payer: payer.map(|value| self.address(value)),
            })),
            (None, None) => Ok(None),
            _ => Err(store("a stored network fee is half written")),
        }
    }

    /// Writes the shared fact of one transaction and answers the place the file
    /// holds it at.
    pub(super) fn insert_fact(
        &self,
        transaction: &Transaction<'_>,
        entry: &CanonicalTransaction,
    ) -> Result<Place, IndexError> {
        let (position, height, hash, parent_position, parent_hash, timestamp) =
            Self::columns(entry.block())?;
        let reason = match &entry.status {
            CanonicalStatus::Failed { reason, .. } => reason.clone(),
            CanonicalStatus::Included { .. } => None,
        };
        let failed = i64::from(matches!(entry.status, CanonicalStatus::Failed { .. }));
        let place = Place::of(entry)?;
        let fee = entry.fee.as_ref();
        let written = transaction
            .prepare_cached(
                "INSERT OR IGNORE INTO transaction_fact
                     (chain, network, transaction_id, block_position, block_height, block_hash,
                      block_parent_position, block_parent_hash, block_timestamp,
                      transaction_order, execution_status, failure_reason,
                      fee_asset, fee_amount, fee_payer)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            )
            .and_then(|mut statement| {
                statement.execute(params![
                    self.chain(),
                    self.name(),
                    place.id,
                    position,
                    height,
                    hash,
                    parent_position,
                    parent_hash,
                    timestamp,
                    place.order,
                    failed,
                    reason,
                    fee.map(|fee| fee.asset.asset.as_str()),
                    fee.map(|fee| fee.amount.to_string()),
                    fee.and_then(|fee| fee.payer.as_ref())
                        .map(|payer| &payer.value),
                ])
            })
            .map_err(|error| failure(&error, "failed to write a transaction"))?;
        if written > 0 {
            return Ok(place);
        }
        self.verify_fact(transaction, entry)
    }

    /// Checks that the transaction the file already holds is the one being
    /// written, and answers the place it holds it at.
    ///
    /// One `(network, transaction)` cannot honestly have two contents, so a
    /// mismatch is corruption and terminal rather than a benign duplicate. The
    /// comparison is per transaction rather than per participating address,
    /// which is one row and one truth.
    fn verify_fact(
        &self,
        transaction: &Transaction<'_>,
        entry: &CanonicalTransaction,
    ) -> Result<Place, IndexError> {
        let identity = entry.transaction_id.id.as_str();
        let held = self.fact(transaction, identity)?;
        let presented = CanonicalTransaction {
            movements: Vec::new(),
            ..entry.clone()
        };
        match held {
            Some((place, stored)) if stored == presented => Ok(place),
            _ => Err(store(
                "a committed transaction disagrees with the stored one",
            )),
        }
    }

    /// Records that `address` took part in the transaction at `place`.
    pub(super) fn insert_membership(
        &self,
        transaction: &Transaction<'_>,
        address: &Address,
        place: &Place,
    ) -> Result<(), IndexError> {
        transaction
            .prepare_cached(
                "INSERT OR IGNORE INTO transaction_address
                     (chain, network, address, block_height, transaction_order,
                      transaction_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .and_then(|mut statement| {
                statement.execute(params![
                    self.chain(),
                    self.name(),
                    address.value,
                    place.height,
                    place.order,
                    place.id,
                ])
            })
            .map(drop)
            .map_err(|error| failure(&error, "failed to write a transaction membership"))
    }
}
