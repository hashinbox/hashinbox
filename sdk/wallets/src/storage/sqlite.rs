//! The `wallet` table on the application's SQLite pool (feature `sqlite`).
//!
//! The table is declared in the one shared schema (`schema/sdk.sql`), which
//! the caller executes on a new file (the server: `migrate`); this store runs statements on that pool and
//! creates nothing. Every statement is the `Wallet` row model's, below.

use base::{Address, ChainName, KeyFormat, Network};
use crypto::SecretBytes;
use serde_json::{Value, json};
use sqlx::{FromRow, Row as _, SqliteConnection, SqlitePool, sqlite::SqliteRow};

use super::{PrivateKey, Registration, StoredWallet, WalletRepository, taken};
use crate::{FutureResult, WalletError, WalletErrorKind};

/// The application's wallets on one SQLite pool. Cheap to clone.
#[derive(Clone)]
pub struct Repository {
    pool: SqlitePool,
}

impl Repository {
    /// Wallets on `pool`, whose file already holds the shared schema. No I/O.
    #[must_use]
    pub const fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn connection(&self) -> Result<sqlx::pool::PoolConnection<sqlx::Sqlite>, WalletError> {
        self.pool.acquire().await.map_err(|error| failure(&error))
    }
}

/// A store failure, without any bound value.
fn failure(error: &sqlx::Error) -> WalletError {
    WalletError::new(
        WalletErrorKind::Unavailable,
        format!(
            "wallet store: {}",
            error.as_database_error().map_or_else(
                || error.to_string(),
                |database| database.message().to_owned()
            )
        ),
    )
}

/// The one row a `get_*` answered, or `None` when there was none.
fn found<T>(fetched: Result<T, sqlx::Error>) -> Result<Option<T>, WalletError> {
    match fetched {
        Ok(row) => Ok(Some(row)),
        Err(sqlx::Error::RowNotFound) => Ok(None),
        Err(error) => Err(failure(&error)),
    }
}

impl WalletRepository for Repository {
    /// One `BEGIN IMMEDIATE` transaction: the address and id are read and
    /// the row filed inside it.
    fn register(&self, registration: Registration) -> FutureResult<'_, StoredWallet> {
        Box::pin(async move {
            if registration.address.value.trim().is_empty() {
                return Err(WalletError::new(
                    WalletErrorKind::InvalidAddress,
                    "a wallet address must not be blank",
                ));
            }
            let row = Wallet::from(registration);
            let mut write = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(|error| failure(&error))?;
            let filed = Self::file(&mut write, row).await?;
            write.commit().await.map_err(|error| failure(&error))?;
            filed.try_into()
        })
    }

    fn wallet<'a>(&'a self, id: &'a str) -> FutureResult<'a, Option<StoredWallet>> {
        Box::pin(async move {
            let mut connection = self.connection().await?;
            found(Wallet::get_by_id(&mut connection, id).await)?
                .map(TryInto::try_into)
                .transpose()
        })
    }

    fn wallets(&self) -> FutureResult<'_, Vec<StoredWallet>> {
        Box::pin(async move {
            let mut connection = self.connection().await?;
            Wallet::find_all(&mut connection)
                .await
                .map_err(|error| failure(&error))?
                .into_iter()
                .map(TryInto::try_into)
                .collect()
        })
    }

    fn archive<'a>(&'a self, id: &'a str) -> FutureResult<'a, Option<StoredWallet>> {
        Box::pin(async move {
            let mut connection = self.connection().await?;
            Wallet::update_archived(&mut connection, id)
                .await
                .map_err(|error| failure(&error))?;
            found(Wallet::get_by_id(&mut connection, id).await)?
                .map(TryInto::try_into)
                .transpose()
        })
    }
}

impl Repository {
    /// Files `row` under the rules of [`WalletRepository::register`].
    async fn file(connection: &mut SqliteConnection, row: Wallet) -> Result<Wallet, WalletError> {
        let held = found(Wallet::get_by_address(&mut *connection, &row).await)?;
        match held {
            Some(held) if held.id() != row.id() => Err(taken("the address is another wallet's")),
            Some(held) => {
                let filed = held.merged(row)?;
                Wallet::update_registration(connection, &filed)
                    .await
                    .map_err(|error| failure(&error))?;
                Ok(filed)
            }
            None => {
                if found(Wallet::get_by_id(&mut *connection, row.id()).await)?.is_some() {
                    return Err(taken("the wallet id names another address"));
                }
                Wallet::insert(connection, &row)
                    .await
                    .map_err(|error| failure(&error))?;
                Ok(row)
            }
        }
    }
}

/// One `wallet` row: a [`Registration`] and its archive stamp. Not
/// comparable as a whole: it may hold a key.
struct Wallet {
    registration: Registration,
    archived_at: Option<i64>,
}

/// A row this build cannot read, without quoting it.
fn malformed(message: &str) -> sqlx::Error {
    sqlx::Error::Decode(message.into())
}

impl FromRow<'_, SqliteRow> for Wallet {
    /// The row; the key is copied straight into zeroizing storage.
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        let format: Option<String> = row.try_get("key_format")?;
        let secret: Option<&[u8]> = row.try_get("private_key")?;
        let key = match (format, secret) {
            (Some(format), Some(secret)) => Some(PrivateKey {
                format: format
                    .parse::<KeyFormat>()
                    .map_err(|_| malformed("a stored key format is unknown"))?,
                secret: SecretBytes::new(secret.to_vec()),
            }),
            (None, None) => None,
            _ => return Err(malformed("a stored wallet key is half written")),
        };
        let metadata: String = row.try_get("metadata")?;
        let metadata: Value = serde_json::from_str(&metadata)
            .map_err(|_| malformed("a stored wallet's metadata is not JSON"))?;
        let label = match &metadata["label"] {
            Value::Null => None,
            Value::String(label) => Some(label.clone()),
            _ => return Err(malformed("a stored wallet label is not text")),
        };
        let network = Network::new(
            ChainName::new(row.try_get::<String, _>("chain")?),
            row.try_get::<String, _>("network")?,
        );
        Ok(Self {
            registration: Registration {
                id: row.try_get("id")?,
                address: Address::new(network, row.try_get::<String, _>("address")?),
                key,
                label,
            },
            archived_at: row.try_get("archived_at")?,
        })
    }
}

impl Wallet {
    /// The row `id` names; `RowNotFound` when none.
    async fn get_by_id(connection: &mut SqliteConnection, id: &str) -> Result<Self, sqlx::Error> {
        sqlx::query_as("SELECT * FROM wallet WHERE id = ?1")
            .bind(id)
            .fetch_one(connection)
            .await
    }

    /// The row holding `row`'s address; `RowNotFound` when none.
    async fn get_by_address(
        connection: &mut SqliteConnection,
        row: &Self,
    ) -> Result<Self, sqlx::Error> {
        sqlx::query_as("SELECT * FROM wallet WHERE chain = ?1 AND network = ?2 AND address = ?3")
            .bind(row.registration.address.network.chain.as_str())
            .bind(&row.registration.address.network.name)
            .bind(&row.registration.address.value)
            .fetch_one(connection)
            .await
    }

    /// Every row, in id order.
    async fn find_all(connection: &mut SqliteConnection) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as("SELECT * FROM wallet ORDER BY id")
            .fetch_all(connection)
            .await
    }

    /// Files `row`, active.
    async fn insert(connection: &mut SqliteConnection, row: &Self) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO wallet (id, chain, network, address, key_format, private_key, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(&row.registration.id)
        .bind(row.registration.address.network.chain.as_str())
        .bind(&row.registration.address.network.name)
        .bind(&row.registration.address.value)
        .bind(row.registration.key.as_ref().map(|key| key.format.as_str()))
        .bind(
            row.registration
                .key
                .as_ref()
                .map(|key| key.secret.as_bytes()),
        )
        .bind(row.metadata())
        .execute(connection)
        .await
        .map(drop)
    }

    /// Writes `row`'s label and key.
    async fn update_registration(
        connection: &mut SqliteConnection,
        row: &Self,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE wallet SET key_format = ?2, private_key = ?3, metadata = ?4 WHERE id = ?1",
        )
        .bind(&row.registration.id)
        .bind(row.registration.key.as_ref().map(|key| key.format.as_str()))
        .bind(
            row.registration
                .key
                .as_ref()
                .map(|key| key.secret.as_bytes()),
        )
        .bind(row.metadata())
        .execute(connection)
        .await
        .map(drop)
    }

    /// Archives the row `id` names now, unless it already is.
    async fn update_archived(
        connection: &mut SqliteConnection,
        id: &str,
    ) -> Result<(), sqlx::Error> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        sqlx::query("UPDATE wallet SET archived_at = ?2 WHERE id = ?1 AND archived_at IS NULL")
            .bind(id)
            .bind(i64::try_from(now).unwrap_or(i64::MAX))
            .execute(connection)
            .await
            .map(drop)
    }

    /// The `metadata` column: the label, when there is one.
    fn metadata(&self) -> String {
        match &self.registration.label {
            Some(label) => json!({ "label": label }),
            None => json!({}),
        }
        .to_string()
    }

    /// The id this row is filed under.
    fn id(&self) -> &str {
        &self.registration.id
    }

    /// This stored row with what `filed` adds: its label when it carries one,
    /// and its key when the row has none.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Conflict`] when both hold a key and they differ.
    fn merged(mut self, filed: Self) -> Result<Self, WalletError> {
        match (&self.registration.key, filed.registration.key) {
            (Some(held), Some(given))
                if held.format != given.format
                    || held.secret.as_bytes() != given.secret.as_bytes() =>
            {
                return Err(taken("this wallet is registered with another key"));
            }
            (None, given) => self.registration.key = given,
            _ => {}
        }
        if filed.registration.label.is_some() {
            self.registration.label = filed.registration.label;
        }
        Ok(self)
    }
}

impl From<Registration> for Wallet {
    fn from(registration: Registration) -> Self {
        Self {
            registration,
            archived_at: None,
        }
    }
}

impl TryFrom<Wallet> for StoredWallet {
    type Error = WalletError;

    fn try_from(row: Wallet) -> Result<Self, WalletError> {
        let archived_at = row
            .archived_at
            .map(u64::try_from)
            .transpose()
            .map_err(|_| {
                WalletError::new(
                    WalletErrorKind::Unavailable,
                    "a stored archive time is negative",
                )
            })?;
        Ok(Self::new(row.registration, archived_at))
    }
}
