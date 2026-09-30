//! The durable watch list: what to observe, and how far it has been observed.
//!
//! **One table, one row per observation.** What to observe and how far it has
//! been inspected used to be two tables joined on every read, which bought a
//! foreign key, a cascade and one extra read per recorded claim. They are one
//! row now, so [`WatchStore::all`]'s "one consistent snapshot" is a single
//! statement with nothing to pair — a watch cannot be handed another watch's
//! progress when there is no second row to fetch it from.
//!
//! `add` is one `BEGIN IMMEDIATE` transaction for the whole batch, so a wallet
//! registered for its native asset and a token either gets both rows or
//! neither. Uniqueness is still the schema's — `(chain, network, asset,
//! address)` is the primary key and `id` carries a unique index over the file —
//! but a *repeat* has to be recognised rather than refused, so the rows a batch
//! touches are read inside that transaction and compared.

use rusqlite::{Connection, Transaction, params};

use super::{Repository, Tables, conflict, failure};
use crate::{
    BlockPosition, BlockRef, BoxFuture, Holder, IndexError, IndexErrorKind, Network, Watch,
    WatchProgress, WatchStore,
};

/// The columns one observation is read as, in the mapping's order. The recorded
/// block reference is six consecutive columns from index four, which is what
/// [`Tables::coordinates`] expects.
const COLUMNS: &str = "id, asset, address, birthday, indexed_position, indexed_height, \
     indexed_hash, indexed_parent_position, indexed_parent_hash, indexed_timestamp";

/// One `watch` row's application half, as the table holds it.
///
/// The network is the request's, so a row does not repeat it, and the chain
/// half of an [`AssetId`] comes back from there rather than from storage.
/// The recorded progress is not here: it is the *index's* half of the row,
/// written by a commit and never by a registration.
struct Observation {
    id: String,
    asset: String,
    address: String,
    birthday: i64,
}

impl WatchStore for Repository {
    /// One transaction for the batch: every watch is read, compared and
    /// inserted inside it, so a conflicting definition anywhere in the batch
    /// leaves no row behind.
    fn add<'a>(&'a self, watches: Vec<Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            let Some(network) = Self::one_network(&watches)? else {
                return Ok(());
            };
            let tables = self.tables(&network)?;
            let rows = watches
                .iter()
                .map(Observation::of)
                .collect::<Result<Vec<_>, _>>()?;
            self.database
                .transaction(move |transaction| tables.register(transaction, &rows))
                .await
        })
    }

    /// The watch list and its progress from one snapshot.
    fn all<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<WatchProgress>, IndexError>> {
        Box::pin(async move {
            let tables = self.tables(network)?;
            self.database
                .blocking(move |connection| tables.observations(connection))
                .await
        })
    }

    /// One delete: the observation and its progress are one row.
    fn forget<'a>(
        &'a self,
        network: &'a Network,
        watch: &'a Watch,
    ) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            if !watch.address.belongs_to(network) {
                return Err(IndexError::new(
                    IndexErrorKind::NetworkMismatch,
                    "watch belongs to another network",
                    false,
                ));
            }
            let tables = self.tables(network)?;
            let id = watch.id.clone();
            self.database
                .transaction(move |transaction| tables.withdraw(transaction, &id))
                .await
        })
    }

    /// One transaction for the batch, and a **narrowed** write inside it: the
    /// birthday it now carries and the claim that birthday invalidates, and
    /// nothing else.
    fn cancel_backfill<'a>(&'a self, watches: Vec<Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            let Some(network) = Self::one_network(&watches)? else {
                return Ok(());
            };
            let tables = self.tables(&network)?;
            let rows = watches
                .iter()
                .map(Observation::of)
                .collect::<Result<Vec<_>, _>>()?;
            self.database
                .transaction(move |transaction| tables.cancel_backfill(transaction, &rows))
                .await
        })
    }
}

impl Repository {
    /// The one network a batch is about, or `None` for an empty batch.
    ///
    /// A batch is atomic, and this store writes one network's rows per
    /// transaction, so a batch spanning two networks is refused rather than
    /// split into two transactions that could half-succeed.
    fn one_network(watches: &[Watch]) -> Result<Option<Network>, IndexError> {
        let Some(first) = watches.first() else {
            return Ok(None);
        };
        let network = first.address.network.clone();
        for watch in watches {
            watch.validate()?;
            if watch.address.network != network {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidRequest,
                    "a batch of watches must all belong to one network",
                    false,
                ));
            }
        }
        Ok(Some(network))
    }
}

impl Tables {
    /// Every observation of this network, with the progress recorded for it, in
    /// one statement.
    fn observations(&self, connection: &Connection) -> Result<Vec<WatchProgress>, IndexError> {
        let selected = format!(
            "SELECT {COLUMNS} FROM watch
             WHERE chain = ?1 AND network = ?2
             ORDER BY asset, address"
        );
        self.observed(connection, &selected, params![self.chain(), self.name()])
    }

    /// Every observation of `address`, whatever its asset, with the progress
    /// recorded for it, read inside the caller's own snapshot.
    ///
    /// What a query of the address's rows is honest with
    /// ([`crate::Inspection`]): the rows and the observations
    /// they were written for come from one transaction, so a claim about how far
    /// the address has been inspected is a claim about the very rows returned.
    /// The `watch_address` index is what keeps it a lookup rather than a walk of
    /// the network's whole watch list on every read.
    pub(super) fn observing(
        &self,
        read: &Connection,
        address: &str,
    ) -> Result<Vec<WatchProgress>, IndexError> {
        let selected = format!(
            "SELECT {COLUMNS} FROM watch
             WHERE chain = ?1 AND network = ?2 AND address = ?3
             ORDER BY asset"
        );
        self.observed(read, &selected, params![self.chain(), self.name(), address])
    }

    /// The observations `selected` names, each with its recorded progress.
    fn observed(
        &self,
        connection: &Connection,
        selected: &str,
        bound: impl rusqlite::Params,
    ) -> Result<Vec<WatchProgress>, IndexError> {
        let mut statement = connection
            .prepare_cached(selected)
            .map_err(|error| failure(&error, "failed to prepare a watch read"))?;
        let mut rows = statement
            .query(bound)
            .map_err(|error| failure(&error, "failed to read the watch list"))?;
        let mut observed = Vec::new();
        while let Some(row) = Self::step(&mut rows)? {
            let held = Observation {
                id: Self::column(row, 0)?,
                asset: Self::column(row, 1)?,
                address: Self::column(row, 2)?,
                birthday: Self::column(row, 3)?,
            };
            observed.push(WatchProgress {
                watch: held.into_watch(self)?,
                indexed_through: self.inspected(row)?,
            });
        }
        Ok(observed)
    }

    /// The six progress columns of one row, or `None` when nothing has been
    /// inspected for it yet.
    fn inspected(&self, row: &rusqlite::Row<'_>) -> Result<Option<BlockRef>, IndexError> {
        if Self::column::<Option<i64>>(row, 4)?.is_none() {
            return Ok(None);
        }
        let held = Self::coordinates(row, 4)
            .map_err(|error| failure(&error, "failed to read recorded progress"))?;
        Self::block(held).map(Some)
    }

    /// Removes the observation `id` names, with its progress.
    fn withdraw(&self, transaction: &Transaction<'_>, id: &str) -> Result<(), IndexError> {
        transaction
            .execute(
                "DELETE FROM watch WHERE chain = ?1 AND network = ?2 AND id = ?3",
                params![self.chain(), self.name(), id],
            )
            .map(drop)
            .map_err(|error| failure(&error, "failed to remove an observation"))
    }

    /// Every row of the batch at the birthday it now carries, with the claim it
    /// earned under the old one gone.
    ///
    /// **Narrowed rather than restricted.** The port's contract is a
    /// postcondition — it states what must be true afterwards, and repeating the
    /// call is how a lost acknowledgement is settled — so refusing a row the
    /// store does not hold would turn that recovery into an unrecoverable state.
    /// What is restricted instead is the *column set*: this writes `birthday`
    /// and the recorded claim and nothing else, so the identity the row is
    /// registered under cannot be rewritten by a re-anchor. The
    /// one precondition is monotonicity — a birthday below the stored one is
    /// refused — which still holds under repeat, because raising to a birthday
    /// already stored is a no-op.
    fn cancel_backfill(
        &self,
        transaction: &Transaction<'_>,
        rows: &[Observation],
    ) -> Result<(), IndexError> {
        for row in rows {
            match self.taken(transaction, row)? {
                Some(held) if held.id == row.id => self.raise(transaction, &held, row)?,
                Some(_) => {
                    return Err(conflict(
                        "an address is already registered for this identity or network",
                    ));
                }
                None => row.insert(transaction, self)?,
            }
        }
        Ok(())
    }

    /// Moves one stored observation to a birthday at or above its own, and
    /// discards the claim the old birthday earned.
    fn raise(
        &self,
        transaction: &Transaction<'_>,
        held: &Observation,
        row: &Observation,
    ) -> Result<(), IndexError> {
        if row.birthday < held.birthday {
            return Err(conflict(
                "a re-anchor must not lower an observation's birthday",
            ));
        }
        transaction
            .execute(
                "UPDATE watch SET birthday = ?4, indexed_position = NULL,
                     indexed_height = NULL, indexed_hash = NULL,
                     indexed_parent_position = NULL, indexed_parent_hash = NULL,
                     indexed_timestamp = NULL
                 WHERE chain = ?1 AND network = ?2 AND id = ?3",
                params![self.chain(), self.name(), row.id, row.birthday],
            )
            .map(drop)
            .map_err(|error| failure(&error, "failed to re-anchor an observation"))
    }

    /// Insert-if-absent, verify-if-present, for the whole batch.
    fn register(
        &self,
        transaction: &Transaction<'_>,
        rows: &[Observation],
    ) -> Result<(), IndexError> {
        for row in rows {
            match self.taken(transaction, row)? {
                Some(stored) => stored.agrees_with(row)?,
                None => row.insert(transaction, self)?,
            }
        }
        Ok(())
    }

    /// The row this observation would replace, addressed by either of the
    /// table's two uniqueness constraints.
    ///
    /// Both are read, because a batch entry can collide on either: the
    /// `(asset, address)` pair it names, or the identity it claims.
    fn taken(
        &self,
        transaction: &Transaction<'_>,
        row: &Observation,
    ) -> Result<Option<Observation>, IndexError> {
        if let Some(held) = self.observation(
            transaction,
            "SELECT id, asset, address, birthday FROM watch
             WHERE chain = ?1 AND network = ?2 AND asset = ?3 AND address = ?4",
            params![self.chain(), self.name(), row.asset, row.address],
        )? {
            return Ok(Some(held));
        }
        self.observation(
            transaction,
            "SELECT id, asset, address, birthday FROM watch
             WHERE chain = ?1 AND network = ?2 AND id = ?3",
            params![self.chain(), self.name(), row.id],
        )
    }

    /// One observation row, or `None`.
    fn observation(
        &self,
        transaction: &Transaction<'_>,
        statement: &'static str,
        bound: impl rusqlite::Params,
    ) -> Result<Option<Observation>, IndexError> {
        transaction
            .query_row(statement, bound, |row| {
                Ok(Observation {
                    id: row.get(0)?,
                    asset: row.get(1)?,
                    address: row.get(2)?,
                    birthday: row.get(3)?,
                })
            })
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                error => Err(failure(&error, "failed to read an observation")),
            })
    }
}

impl Observation {
    /// The row `watch` is stored as.
    ///
    /// A birthday above `i64::MAX` is refused rather than wrapped, and refused
    /// as the caller's mistake: no chain has such a block, so the only way to
    /// ask for one is to ask wrongly, and a caller can fix a 400 where it can
    /// do nothing with a 500.
    fn of(watch: &Watch) -> Result<Self, IndexError> {
        let birthday = i64::try_from(watch.start.0).map_err(|_| {
            IndexError::new(
                IndexErrorKind::InvalidRequest,
                "a watched start position is above the storable range",
                false,
            )
        })?;
        Ok(Self {
            id: watch.id.clone(),
            asset: watch.asset.asset.clone(),
            address: watch.address.value.clone(),
            birthday,
        })
    }

    /// The observation this row holds, in the read's network.
    fn into_watch(self, tables: &Tables) -> Result<Watch, IndexError> {
        Ok(Watch {
            id: self.id,
            address: tables.address(self.address),
            asset: tables.asset(self.asset),
            start: BlockPosition(Tables::restored(self.birthday)?),
        })
    }

    /// Accepts `row` only as an exact repeat of this stored one.
    ///
    /// Every field, because every field is part of the observation's meaning: a
    /// different birthday is a different range to inspect, and a different id,
    /// asset or address is a different observation wearing a name that is taken.
    fn agrees_with(&self, row: &Self) -> Result<(), IndexError> {
        if self.id == row.id
            && self.asset == row.asset
            && self.address == row.address
            && self.birthday == row.birthday
        {
            return Ok(());
        }
        Err(conflict(
            "an address is already registered for this identity or network",
        ))
    }

    /// Adds this row to `tables`' network, with nothing inspected for it.
    ///
    /// Seeds the persisted total from retained effects in this same transaction.
    fn insert(&self, transaction: &Transaction<'_>, tables: &Tables) -> Result<(), IndexError> {
        transaction
            .execute(
                "INSERT INTO watch (chain, network, asset, address, id, birthday)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    tables.chain(),
                    tables.name(),
                    self.asset,
                    self.address,
                    self.id,
                    self.birthday,
                ],
            )
            .map(drop)
            .map_err(Self::taken_already)?;
        tables.seed_total(transaction, (&self.address, &self.asset))
    }

    /// A uniqueness the read inside this transaction did not see: the identity
    /// is taken in **another** network of the same file, which the unique index
    /// on `id` spans deliberately.
    fn taken_already(error: rusqlite::Error) -> IndexError {
        if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
            return IndexError::new(
                IndexErrorKind::Conflict,
                "an address is already registered for this identity or network",
                false,
            );
        }
        failure(&error, "failed to write an observation")
    }
}

/// What the shared watch-store contract cannot state, because none of it is a
/// property of the port (`tests/repository_contract.rs` runs that contract
/// against this store and the in-memory one together).
#[cfg(test)]
mod watch_store_test {
    use tempfile::TempDir;

    use super::super::database_test::{block_on, network, open};
    use super::*;
    use crate::{Address, AssetId, ChainName, IndexErrorKind};

    fn watch(network: &Network, id: &str) -> Watch {
        Watch {
            id: id.to_owned(),
            address: Address::new(network.clone(), "a"),
            asset: AssetId::native(network.chain.clone()),
            start: BlockPosition(3),
        }
    }

    /// An observation survives reopening the *file*, and an identity is unique
    /// across every network in it.
    #[test]
    fn registrations_survive_reopening_the_file() {
        let directory = TempDir::new().expect("temporary directory");
        let repository = open(&directory, "shared.db");
        let elsewhere = Network::new(ChainName::new("other"), "testnet");
        block_on(repository.add(vec![watch(&network(), "mine")])).expect("mine");
        block_on(repository.add(vec![watch(&elsewhere, "theirs")]))
            .expect("same address elsewhere");
        let error =
            block_on(repository.add(vec![watch(&elsewhere, "mine")])).expect_err("global id");
        assert_eq!(error.kind, IndexErrorKind::Conflict);
        drop(repository);

        let reopened = open(&directory, "shared.db");
        let stored = block_on(reopened.all(&network())).expect("observations");
        assert_eq!(stored, [WatchProgress::unread(watch(&network(), "mine"))]);
    }
}
