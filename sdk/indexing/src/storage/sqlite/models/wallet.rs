//! `wallet`: every wallet the store holds, observed or archived. One row
//! carries the registration — identity, holding, birthday, label, key and
//! archive stamp — and what commits record for an observed one: how far it is
//! inspected and its persisted total.
//!
//! An observed row has `archived_at` null. Every read that scanning,
//! inspection, the progress compare-and-set, receipt novelty and total upkeep
//! make is narrowed to those rows and never selects the label or the key, so
//! none of them sees an archived row or holds a secret.

use crypto::SecretBytes;
use num_bigint::BigInt;
use sqlx::{FromRow, QueryBuilder, SqliteConnection, error::ErrorKind, sqlite::SqliteRow};

use super::{
    Block, Chunks, Deltas, Ledger, column, failure, fetched, framed, network, restored, store,
};
use crate::storage::{wallet, watch::redefinition};
use crate::{
    Address, AssetId, BlockPosition, BlockRef, Completeness, Holder, IndexError, IndexErrorKind,
    Inspection, Network, Registration, Watch, WatchProgress,
};

/// A statement over observed rows: every column but the application's label
/// and key, which read as null, and then `$tail` after `FROM wallet`.
macro_rules! observed {
    ($tail:literal) => {
        concat!(
            "SELECT chain, network, asset, address, id, birthday, block_position, \
             block_height, block_hash, block_parent_position, block_parent_hash, \
             block_timestamp, balance, archived_at, NULL AS label, NULL AS secret \
             FROM wallet ",
            $tail
        )
    };
}

/// One `wallet` row.
///
/// Not comparable as a whole: it may hold a key, and the comparisons a write
/// makes are narrower — the two names ([`Self::holds`]), an exact repeat of an
/// observation ([`Self::repeats`]), and the key ([`Self::merged`]).
pub(in crate::storage::sqlite) struct Wallet {
    network: Network,
    asset: String,
    address: String,
    id: String,
    birthday: Option<i64>,
    indexed: Option<Block>,
    balance: Option<String>,
    label: Option<String>,
    secret: Option<SecretBytes>,
    archived_at: Option<i64>,
}

impl FromRow<'_, SqliteRow> for Wallet {
    /// The row; the key is copied from it straight into zeroizing storage.
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            network: network(row)?,
            asset: column(row, "asset")?,
            address: column(row, "address")?,
            id: column(row, "id")?,
            birthday: column(row, "birthday")?,
            indexed: Block::optional(row)?,
            balance: column(row, "balance")?,
            label: column(row, "label")?,
            secret: column::<Option<&[u8]>>(row, "secret")?
                .map(|secret| SecretBytes::new(secret.to_vec())),
            archived_at: column(row, "archived_at")?,
        })
    }
}

impl Wallet {
    /// The statement [`Self::find_observed`] runs.
    pub(in crate::storage::sqlite) const OBSERVED: &str = observed!(
        "WHERE chain = ?1 AND network = ?2 AND archived_at IS NULL ORDER BY asset, address"
    );

    /// The statement [`Self::find_observed_by_address`] runs.
    ///
    /// The address's keys come from `wallet_address` and each one's row from
    /// the primary key. Left to choose, SQLite without statistics walks the
    /// network's whole primary key instead, since its default estimates rate
    /// `(chain, network)` nearly as selective as `(chain, network, address)`:
    /// a read that grows with every wallet the network holds. `INDEXED BY`
    /// fixes the choice whatever statistics a file carries.
    pub(in crate::storage::sqlite) const OBSERVED_BY_ADDRESS: &str = observed!(
        "INDEXED BY wallet_address
         WHERE chain = ?1 AND network = ?2 AND address = ?3 AND archived_at IS NULL
         ORDER BY asset"
    );

    /// The statement [`Self::get_observed_by_id`] runs.
    pub(in crate::storage::sqlite) const OBSERVED_BY_ID: &str =
        observed!("WHERE chain = ?1 AND network = ?2 AND id = ?3 AND archived_at IS NULL");

    /// The statement [`Self::get_observed_by_subject`] runs.
    pub(in crate::storage::sqlite) const OBSERVED_BY_SUBJECT: &str = observed!(
        "WHERE chain = ?1 AND network = ?2 AND address = ?3 AND asset = ?4
           AND archived_at IS NULL"
    );

    /// The row `id` names, in whichever network; `RowNotFound` when none.
    pub(in crate::storage::sqlite) async fn get_by_id(
        connection: &mut SqliteConnection,
        id: &str,
    ) -> Result<Self, sqlx::Error> {
        sqlx::query_as("SELECT * FROM wallet WHERE id = ?1")
            .bind(id)
            .fetch_one(connection)
            .await
    }

    /// The row `network` holds under `id`, observed or archived;
    /// `RowNotFound` when none.
    pub(in crate::storage::sqlite) async fn get_in_network(
        connection: &mut SqliteConnection,
        network: &Network,
        id: &str,
    ) -> Result<Self, sqlx::Error> {
        sqlx::query_as("SELECT * FROM wallet WHERE chain = ?1 AND network = ?2 AND id = ?3")
            .bind(network.chain.as_str())
            .bind(&network.name)
            .bind(id)
            .fetch_one(connection)
            .await
    }

    /// The row that holds `(address, asset)` in `network`, observed or
    /// archived; `RowNotFound` when none.
    pub(in crate::storage::sqlite) async fn get_by_subject(
        connection: &mut SqliteConnection,
        network: &Network,
        (address, asset): (&str, &str),
    ) -> Result<Self, sqlx::Error> {
        sqlx::query_as(
            "SELECT * FROM wallet
             WHERE chain = ?1 AND network = ?2 AND asset = ?3 AND address = ?4",
        )
        .bind(network.chain.as_str())
        .bind(&network.name)
        .bind(asset)
        .bind(address)
        .fetch_one(connection)
        .await
    }

    /// The observed row `network` holds under `id`; `RowNotFound` when none.
    pub(in crate::storage::sqlite) async fn get_observed_by_id(
        connection: &mut SqliteConnection,
        network: &Network,
        id: &str,
    ) -> Result<Self, sqlx::Error> {
        sqlx::query_as(Self::OBSERVED_BY_ID)
            .bind(network.chain.as_str())
            .bind(&network.name)
            .bind(id)
            .fetch_one(connection)
            .await
    }

    /// The observed row of `(address, asset)` in `network`; `RowNotFound`
    /// when the holding is not observed — never registered, or archived.
    pub(in crate::storage::sqlite) async fn get_observed_by_subject(
        connection: &mut SqliteConnection,
        network: &Network,
        (address, asset): (&str, &str),
    ) -> Result<Self, sqlx::Error> {
        sqlx::query_as(Self::OBSERVED_BY_SUBJECT)
            .bind(network.chain.as_str())
            .bind(&network.name)
            .bind(address)
            .bind(asset)
            .fetch_one(connection)
            .await
    }

    /// Every row of the file, in id order.
    pub(in crate::storage::sqlite) async fn find_all(
        connection: &mut SqliteConnection,
    ) -> Result<Vec<Self>, IndexError> {
        let query = sqlx::query("SELECT * FROM wallet ORDER BY id");
        fetched(connection, query, "failed to read the wallets").await
    }

    /// Every observed row of `network`, in key order: the scan list.
    pub(in crate::storage::sqlite) async fn find_observed(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> Result<Vec<Self>, IndexError> {
        let query = framed(Self::OBSERVED, network);
        fetched(connection, query, "failed to read the watch list").await
    }

    /// Every observed row of `address`, whatever its asset, read inside the
    /// caller's own snapshot.
    ///
    /// What a query of the address's rows is honest with ([`Inspection`]): the
    /// rows and the observations they were written for come from one
    /// transaction, so a claim about how far the address has been inspected is
    /// a claim about the very rows returned.
    pub(in crate::storage::sqlite) async fn find_observed_by_address(
        connection: &mut SqliteConnection,
        network: &Network,
        address: &str,
    ) -> Result<Vec<Self>, IndexError> {
        let query = framed(Self::OBSERVED_BY_ADDRESS, network).bind(address);
        fetched(connection, query, "failed to read the watch list").await
    }

    /// The observed rows of `network` among `holdings`, many to a statement.
    async fn find_observed_by_subjects(
        connection: &mut SqliteConnection,
        network: &Network,
        subjects: &[&(String, String)],
    ) -> Result<Vec<Self>, IndexError> {
        Chunks::new(subjects, "failed to read watched balances")
            .fetch(connection, |chunk| {
                let mut statement = QueryBuilder::new(observed!("WHERE chain = "));
                statement
                    .push_bind(network.chain.as_str())
                    .push(" AND network = ")
                    .push_bind(network.name.as_str())
                    .push(" AND archived_at IS NULL AND (address, asset) IN (");
                statement.push_values(chunk, |mut subject, (address, asset)| {
                    subject
                        .push_bind(address.as_str())
                        .push_bind(asset.as_str());
                });
                statement.push(")");
                statement
            })
            .await
    }

    /// What the observations of `of`'s address can claim about the history
    /// behind its rows, at the `checkpoint` of the snapshot `connection` holds.
    ///
    /// **Answered once for both reads.** A balance and a history page are two
    /// projections of one address's stored rows, so what the store can prove
    /// about the range behind them is one fact, read here inside whichever
    /// read's own snapshot asked for it. Two copies of this rule could
    /// disagree, and a caller comparing a complete-looking page against an
    /// incomplete-looking total would have no way to tell which of them was
    /// lying.
    ///
    /// **Computed, never stored.** A whole-history claim is a conjunction of
    /// facts the rows already carry and the line this snapshot was read at:
    /// observation began at the chain's first block, progress has been
    /// recorded for it, **and** no observation of the address is still behind
    /// that line. The rule is [`Inspection`]'s and both stores ask it; what is
    /// this store's own is that the rows it is asked over come from the same
    /// transaction as the answer they qualify, which is why no index has to
    /// repair the claim afterwards.
    pub(in crate::storage::sqlite) async fn completeness(
        connection: &mut SqliteConnection,
        network: &Network,
        of: &(dyn Holder + Sync),
        checkpoint: Option<&BlockRef>,
    ) -> Result<Completeness, IndexError> {
        let observations = Self::find_observed_by_address(connection, network, &of.address().value)
            .await?
            .into_iter()
            .map(WatchProgress::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Inspection::new(&observations, checkpoint).completeness(of))
    }

    /// Adds `row` with nothing inspected for it: observed with its total seeded
    /// from retained effects in this same transaction, or archived with none.
    pub(in crate::storage::sqlite) async fn insert(
        connection: &mut SqliteConnection,
        row: &Self,
    ) -> Result<(), IndexError> {
        let balance = (!row.archived()).then_some("0");
        sqlx::query(
            "INSERT INTO wallet (chain, network, asset, address, id, birthday, balance,
                 label, secret, archived_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )
        .bind(row.network.chain.as_str())
        .bind(&row.network.name)
        .bind(&row.asset)
        .bind(&row.address)
        .bind(&row.id)
        .bind(row.birthday)
        .bind(balance)
        .bind(row.label.as_deref())
        .bind(row.secret.as_ref().map(SecretBytes::as_bytes))
        .bind(row.archived_at)
        .execute(&mut *connection)
        .await
        .map_err(|error| Self::taken_already(&error))?;
        if row.archived() {
            return Ok(());
        }
        Self::update_seeded(connection, row).await
    }

    /// Observes the stored row `row` names again, from `row`'s birthday, with
    /// nothing inspected and its total seeded from retained effects in this
    /// same transaction.
    pub(in crate::storage::sqlite) async fn reactivate(
        connection: &mut SqliteConnection,
        row: &Self,
    ) -> Result<(), IndexError> {
        sqlx::query(
            "UPDATE wallet SET birthday = ?4, archived_at = NULL, balance = '0'
             WHERE chain = ?1 AND network = ?2 AND id = ?3",
        )
        .bind(row.network.chain.as_str())
        .bind(&row.network.name)
        .bind(&row.id)
        .bind(row.birthday)
        .execute(&mut *connection)
        .await
        .map_err(|error| failure(&error, "failed to observe a wallet again"))?;
        Self::update_seeded(connection, row).await
    }

    /// Seeds `row`'s persisted total from its holding's retained effects.
    async fn update_seeded(
        connection: &mut SqliteConnection,
        row: &Self,
    ) -> Result<(), IndexError> {
        let total = Ledger::net(connection, &row.network, row.subject()).await?;
        Self::update_balance(connection, &row.network, row.subject(), total).await
    }

    /// Writes `row`'s label and key, the application's half of it.
    pub(in crate::storage::sqlite) async fn update_registration(
        connection: &mut SqliteConnection,
        row: &Self,
    ) -> Result<(), IndexError> {
        sqlx::query(
            "UPDATE wallet SET label = ?4, secret = ?5
             WHERE chain = ?1 AND network = ?2 AND id = ?3",
        )
        .bind(row.network.chain.as_str())
        .bind(&row.network.name)
        .bind(&row.id)
        .bind(row.label.as_deref())
        .bind(row.secret.as_ref().map(SecretBytes::as_bytes))
        .execute(connection)
        .await
        .map(drop)
        .map_err(|error| failure(&error, "failed to file a wallet"))
    }

    /// Archives the observed row `network` holds under `id`, if any: its claim
    /// and total go, its birthday, label, key and every indexed fact stay.
    pub(in crate::storage::sqlite) async fn archive(
        connection: &mut SqliteConnection,
        network: &Network,
        id: &str,
    ) -> Result<(), IndexError> {
        let at = i64::try_from(wallet::now()).unwrap_or(i64::MAX);
        sqlx::query(
            "UPDATE wallet SET archived_at = ?4, balance = NULL,
                 block_position = NULL, block_height = NULL, block_hash = NULL,
                 block_parent_position = NULL, block_parent_hash = NULL,
                 block_timestamp = NULL
             WHERE chain = ?1 AND network = ?2 AND id = ?3 AND archived_at IS NULL",
        )
        .bind(network.chain.as_str())
        .bind(&network.name)
        .bind(id)
        .bind(at)
        .execute(connection)
        .await
        .map(drop)
        .map_err(|error| failure(&error, "failed to archive an observation"))
    }

    /// Moves `held`, the stored row `row` re-anchors, to `row`'s birthday, and
    /// discards the claim the old birthday earned.
    ///
    /// The one precondition is monotonicity: a birthday below the stored one is
    /// refused for good — the stored birthday is the caller's own, so a retry
    /// is refused again — which still holds under repeat, because raising to a
    /// birthday already stored is a no-op.
    pub(in crate::storage::sqlite) async fn reanchor(
        connection: &mut SqliteConnection,
        row: &Self,
        held: &Self,
    ) -> Result<(), IndexError> {
        if row.birthday < held.birthday {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "a re-anchor must not lower an observation's birthday",
                false,
            ));
        }
        sqlx::query(
            "UPDATE wallet SET birthday = ?4, block_position = NULL,
                 block_height = NULL, block_hash = NULL,
                 block_parent_position = NULL, block_parent_hash = NULL,
                 block_timestamp = NULL
             WHERE chain = ?1 AND network = ?2 AND id = ?3",
        )
        .bind(row.network.chain.as_str())
        .bind(&row.network.name)
        .bind(&row.id)
        .bind(row.birthday)
        .execute(connection)
        .await
        .map(drop)
        .map_err(|error| failure(&error, "failed to re-anchor an observation"))
    }

    /// Records that the observation `network` holds under `id` is inspected
    /// through `through`.
    pub(in crate::storage::sqlite) async fn update_progress(
        connection: &mut SqliteConnection,
        network: &Network,
        id: &str,
        through: &Block,
    ) -> Result<(), IndexError> {
        let query = framed(
            "UPDATE wallet SET block_position = ?4, block_height = ?5,
                 block_hash = ?6, block_parent_position = ?7,
                 block_parent_hash = ?8, block_timestamp = ?9
             WHERE chain = ?1 AND network = ?2 AND id = ?3 AND archived_at IS NULL",
            network,
        )
        .bind(id);
        through
            .bind(query)
            .execute(connection)
            .await
            .map(drop)
            .map_err(|error| failure(&error, "failed to record indexed progress"))
    }

    /// Adds what one commit's written effects change each holding by to its
    /// persisted total, inside that commit's transaction.
    ///
    /// Once per holding rather than once per row: the observed holdings among
    /// `deltas` are read many to a statement, and each one's total is written
    /// once. A holding that is not observed has no total, and its reads fold
    /// the ledger instead.
    pub(in crate::storage::sqlite) async fn update_balances(
        connection: &mut SqliteConnection,
        network: &Network,
        deltas: &Deltas,
    ) -> Result<(), IndexError> {
        let subjects: Vec<_> = deltas.keys().collect();
        let observed = Self::find_observed_by_subjects(connection, network, &subjects).await?;
        for row in observed {
            let Some(delta) = deltas.get(&(row.address.clone(), row.asset.clone())) else {
                continue;
            };
            let Some(held) = row.balance()? else {
                continue;
            };
            Self::update_balance(connection, network, row.subject(), held + delta).await?;
        }
        Ok(())
    }

    /// Writes one observed holding's persisted total.
    async fn update_balance(
        connection: &mut SqliteConnection,
        network: &Network,
        (address, asset): (&str, &str),
        total: BigInt,
    ) -> Result<(), IndexError> {
        framed(
            "UPDATE wallet SET balance = ?5
             WHERE chain = ?1 AND network = ?2 AND address = ?3 AND asset = ?4
                 AND archived_at IS NULL",
            network,
        )
        .bind(address)
        .bind(asset)
        .bind(total.to_string())
        .execute(connection)
        .await
        .map(drop)
        .map_err(|error| failure(&error, "failed to write a watched balance"))
    }

    /// Resets everything every row of `network` claimed, because the rows
    /// behind all of it have just been purged: progress on every row, and the
    /// total of every observed one. Archived rows hold neither, and keep their
    /// archive, label and key, as every row keeps its birthday.
    pub(in crate::storage::sqlite) async fn purge(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> Result<(), IndexError> {
        framed(
            "UPDATE wallet SET
                 balance = CASE WHEN archived_at IS NULL THEN '0' END,
                 block_position = NULL, block_height = NULL, block_hash = NULL,
                 block_parent_position = NULL, block_parent_hash = NULL,
                 block_timestamp = NULL
             WHERE chain = ?1 AND network = ?2",
            network,
        )
        .execute(connection)
        .await
        .map(drop)
        .map_err(|error| failure(&error, "failed to reset a purged network's claims"))
    }

    /// The `(address, asset)` holding this row observes.
    pub(in crate::storage::sqlite) fn subject(&self) -> (&str, &str) {
        (&self.address, &self.asset)
    }

    /// The identity this row is registered under.
    pub(in crate::storage::sqlite) fn id(&self) -> &str {
        &self.id
    }

    /// Whether nothing scans this row.
    pub(in crate::storage::sqlite) const fn archived(&self) -> bool {
        self.archived_at.is_some()
    }

    /// How far this row is inspected; `None` when nothing has been recorded.
    pub(in crate::storage::sqlite) fn progress(&self) -> Result<Option<BlockRef>, IndexError> {
        self.indexed.clone().map(BlockRef::try_from).transpose()
    }

    /// The persisted total; `None` on an archived row, whose reads fold the
    /// ledger instead.
    pub(in crate::storage::sqlite) fn balance(&self) -> Result<Option<BigInt>, IndexError> {
        self.balance
            .as_deref()
            .map(|value| {
                value
                    .parse()
                    .map_err(|_| store("a stored balance is not an atomic integer"))
            })
            .transpose()
    }

    /// Whether `other` is this very registration under the same identity and
    /// holding, whatever birthday either carries.
    pub(in crate::storage::sqlite) fn holds(&self, other: &Self) -> bool {
        self.id == other.id && self.subject() == other.subject()
    }

    /// Whether `observation` re-states this observed row exactly: same names
    /// and birthday. An archived row repeats nothing; it is reactivated.
    pub(in crate::storage::sqlite) fn repeats(&self, observation: &Self) -> bool {
        !self.archived() && self.holds(observation) && self.birthday == observation.birthday
    }

    /// This stored row with what `filed` adds: its label when it carries one,
    /// and its key when the row has none.
    ///
    /// # Errors
    ///
    /// A non-retryable `Conflict` when both hold a key and they differ.
    pub(in crate::storage::sqlite) fn merged(mut self, filed: Self) -> Result<Self, IndexError> {
        match (&self.secret, filed.secret) {
            (Some(held), Some(given)) if held.as_bytes() != given.as_bytes() => {
                return Err(wallet::another_key());
            }
            (None, given) => self.secret = given,
            _ => {}
        }
        if filed.label.is_some() {
            self.label = filed.label;
        }
        Ok(self)
    }

    /// A uniqueness the read inside this transaction did not see: the identity
    /// is taken in **another** network of the same file, which the unique index
    /// on `id` spans deliberately.
    fn taken_already(error: &sqlx::Error) -> IndexError {
        let unique = error
            .as_database_error()
            .is_some_and(|database| database.kind() == ErrorKind::UniqueViolation);
        if unique {
            return redefinition();
        }
        failure(error, "failed to write a wallet")
    }
}

impl TryFrom<Wallet> for WatchProgress {
    type Error = IndexError;

    /// The observation an observed row holds, with the progress recorded for
    /// it; an observed row always has its birthday.
    fn try_from(row: Wallet) -> Result<Self, IndexError> {
        let birthday = row
            .birthday
            .ok_or_else(|| store("an observed wallet has no birthday"))?;
        let indexed_through = row.progress()?;
        Ok(Self {
            watch: Watch {
                id: row.id,
                asset: AssetId {
                    chain: row.network.chain.clone(),
                    asset: row.asset,
                },
                address: Address {
                    network: row.network,
                    value: row.address,
                },
                start: BlockPosition(restored(birthday)?),
            },
            indexed_through,
        })
    }
}

impl TryFrom<&Watch> for Wallet {
    type Error = IndexError;

    /// An observed row, with no label and no key. A birthday above `i64::MAX`
    /// is refused rather than wrapped, and refused as the caller's mistake: no
    /// chain has such a block, so the only way to ask for one is to ask
    /// wrongly, and a caller can fix a 400 where it can do nothing with a 500.
    fn try_from(watch: &Watch) -> Result<Self, IndexError> {
        let birthday = i64::try_from(watch.start.0).map_err(|_| {
            IndexError::new(
                IndexErrorKind::InvalidRequest,
                "a watched start position is above the storable range",
                false,
            )
        })?;
        Ok(Self {
            network: watch.address.network.clone(),
            asset: watch.asset.asset.clone(),
            address: watch.address.value.clone(),
            id: watch.id.clone(),
            birthday: Some(birthday),
            indexed: None,
            balance: None,
            label: None,
            secret: None,
            archived_at: None,
        })
    }
}

impl From<Registration> for Wallet {
    /// A filed wallet nothing observes yet: archived now, with no birthday.
    fn from(registration: Registration) -> Self {
        Self {
            network: registration.address.network,
            asset: registration.asset.asset,
            address: registration.address.value,
            id: registration.id,
            birthday: None,
            indexed: None,
            balance: None,
            label: registration.label,
            secret: registration.secret,
            archived_at: Some(i64::try_from(wallet::now()).unwrap_or(i64::MAX)),
        }
    }
}

impl TryFrom<Wallet> for crate::Wallet {
    type Error = IndexError;

    fn try_from(row: Wallet) -> Result<Self, IndexError> {
        let birthday = row
            .birthday
            .map(|birthday| restored(birthday).map(BlockPosition))
            .transpose()?;
        let archived_at = row
            .archived_at
            .map(|at| u64::try_from(at).map_err(|_| store("a stored archive time is negative")))
            .transpose()?;
        let registration = Registration {
            id: row.id,
            asset: AssetId {
                chain: row.network.chain.clone(),
                asset: row.asset,
            },
            address: Address {
                network: row.network,
                value: row.address,
            },
            label: row.label,
            secret: row.secret,
        };
        Self::new(registration, birthday, archived_at)
    }
}

/// What the shared write contract below the line cannot be asked, because only a
/// store that keeps movements as their own rows has them: the `ledger`
/// rows a refused batch must also roll back.
///
/// The contract itself runs in `tests/repository_contract.rs`, against this
/// store and the in-memory one together, which is the only definition this
/// repository has of two stores being the same store.
#[cfg(test)]
mod progress_test {
    use base::Decimal;
    use tempfile::TempDir;

    use super::super::super::{
        Repository,
        database_test::{network, open},
    };
    use crate::{
        Address, AssetId, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef,
        BlockWithOutputs, FeeSettlement, IndexErrorKind, IndexStore, IndexedBatch, MovementId,
        NetworkFee, Progress, Status, Transaction, TransactionRef, ValueMovement, Watch,
        WatchStore,
    };

    fn block(height: u64) -> BlockRef {
        let hash = |height: u64| BlockHash(height.to_be_bytes().to_vec());
        BlockRef {
            position: BlockPosition(height),
            height: BlockHeight(height),
            hash: hash(height),
            parent: Some(BlockParent {
                position: BlockPosition(height - 1),
                hash: hash(height - 1),
            }),
            timestamp: Some(1_000 + height),
        }
    }

    fn address(value: &str) -> Address {
        Address::new(network(), value)
    }

    /// One block as the interpreter reports it for the address that was not
    /// watched when it was committed; `amount` is what makes a re-presented
    /// block disagree with the stored one.
    fn canonical(height: u64, amount: u64) -> BlockWithOutputs {
        let asset = AssetId::native(network().chain);
        let draft = Transaction {
            id: TransactionRef::new(network(), format!("block-{height}")),
            movements: vec![ValueMovement::transfer(
                MovementId(format!("m-{height}")),
                asset.clone(),
                Decimal::from(amount),
                address("payer"),
                address("late"),
            )],
            fee: Some(NetworkFee {
                asset,
                amount: Decimal::from(1_u64),
                payer: Some(address("payer")),
            }),
            order: 0,
            status: Status::Included {
                block: block(height),
            },
        };
        BlockWithOutputs::new(&network(), block(height), vec![draft])
            .expect("unique transaction ids")
    }

    /// How many rows one table holds.
    async fn rows(repository: &Repository, table: &'static str) -> u64 {
        let statement = format!("SELECT count(*) FROM {table}");
        let count: i64 = sqlx::query_scalar(&statement)
            .fetch_one(repository.database.pool())
            .await
            .expect("row count");
        u64::try_from(count).expect("a count")
    }

    /// **Atomicity, in the rows only this backend keeps separately.** The shared
    /// contract asserts a refused batch writes no history row and records no
    /// claim; this asserts the `movement` rows behind those history rows go with
    /// them, which a store that nests movements inside its history entry cannot
    /// be asked.
    #[tokio::test]
    async fn a_refused_batch_rolls_back_its_movement_rows_too() {
        let directory = TempDir::new().expect("temporary directory");
        let repository = open(&directory, "atomic.db").await;
        let late = Watch {
            id: "late".to_owned(),
            address: address("late"),
            asset: AssetId::native(network().chain),
            start: BlockPosition(1),
        };
        repository
            .add(vec![late.clone()])
            .await
            .expect("the observation");
        let mut checkpoint = None;
        for height in 1..=5 {
            let empty = BlockWithOutputs::new(&network(), block(height), Vec::new())
                .expect("unique transaction ids");
            let batch = IndexedBatch::new(
                network(),
                checkpoint,
                vec![empty],
                Vec::new(),
                FeeSettlement::Charged,
            )
            .expect("valid");
            repository.commit(batch).await.expect("fixture block");
            checkpoint = Some(block(height));
        }
        let first = IndexedBatch::new(
            network(),
            Some(block(5)),
            vec![canonical(5, 10)],
            vec![Progress {
                watch_id: late.id.clone(),
                expected: None,
                indexed_through: block(5),
            }],
            FeeSettlement::Charged,
        )
        .expect("a valid batch");
        repository.commit(first).await.expect("the first batch");
        let before = [
            rows(&repository, "transaction_address").await,
            rows(&repository, "ledger").await,
        ];

        // Blocks 3 and 4 are new rows; block 5 is already stored and this batch
        // presents it with a different amount.
        let disagreeing = IndexedBatch::new(
            network(),
            Some(block(5)),
            vec![canonical(3, 10), canonical(4, 10), canonical(5, 99)],
            Vec::new(),
            FeeSettlement::Charged,
        )
        .expect("a valid batch");
        let error = repository
            .commit(disagreeing)
            .await
            .expect_err("a disagreeing row");

        assert_eq!(error.kind, IndexErrorKind::Store);
        assert!(!error.retryable, "retrying cannot repair the store");
        assert_eq!(
            [
                rows(&repository, "transaction_address").await,
                rows(&repository, "ledger").await
            ],
            before,
            "the rows of blocks 3 and 4 were written before the refusal and must be gone",
        );
        assert_eq!(
            repository
                .all(&network())
                .await
                .expect("observations")
                .into_iter()
                .filter_map(|observed| observed.indexed_through)
                .collect::<Vec<_>>(),
            [block(5)],
            "and the claim the first batch proved is untouched",
        );
    }
}
