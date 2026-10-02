//! Application-owned wallet keys in the same SQLite file as indexing.

use rusqlite::{Connection, OptionalExtension, params};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use wallets::{SecretBytes, WalletError, WalletErrorKind};

use crate::api::{AssetTag, Signers};

/// Durable signing material. Neither this value nor its rows is serializable.
pub(crate) struct Registry {
    connection: Mutex<Connection>,
    admission: Arc<tokio::sync::Semaphore>,
}

/// A private boot row; public identity remains owned by the durable watch.
struct Record {
    id: String,
    tag: AssetTag,
    material: SecretBytes,
}

impl Registry {
    // Application version never changes indexing's PRAGMA user_version.
    const SCHEMA: &str = "CREATE TABLE wallet_schema (
        id INTEGER PRIMARY KEY CHECK(id = 1), version INTEGER NOT NULL);
        INSERT INTO wallet_schema VALUES (1, 2);
        CREATE TABLE wallet (
        id TEXT PRIMARY KEY, tag TEXT NOT NULL, secret BLOB NOT NULL) WITHOUT ROWID;";

    /// Opens plaintext application tables after the indexing file is initialized.
    pub(crate) async fn open(path: &Path) -> Result<Arc<Self>, WalletError> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let connection = Connection::open(path)
                .map_err(|_| Self::failure("wallet registry could not open"))?;
            connection
                .busy_timeout(std::time::Duration::from_secs(5))
                .map_err(|_| Self::failure("wallet registry could not set its timeout"))?;
            connection
                .execute_batch("PRAGMA synchronous = FULL; PRAGMA fullfsync = ON;")
                .map_err(|_| Self::failure("wallet registry could not set durability"))?;
            Self::from_connection(connection).map(Arc::new)
        })
        .await
        .map_err(|_| Self::failure("wallet registry initialization stopped"))?
    }

    fn from_connection(mut connection: Connection) -> Result<Self, WalletError> {
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|_| Self::failure("wallet registry could not begin initialization"))?;
        let exists: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'wallet_schema')",
                [],
                |row| row.get(0),
            )
            .map_err(|_| Self::failure("wallet schema could not read"))?;
        if exists {
            let version: u32 = transaction
                .query_row(
                    "SELECT version FROM wallet_schema WHERE id = 1",
                    [],
                    |row| row.get(0),
                )
                .map_err(|_| Self::failure("wallet registry metadata is invalid"))?;
            if version != 2 {
                return Err(Self::failure("unsupported wallet registry schema"));
            }
        } else {
            transaction
                .execute_batch(Self::SCHEMA)
                .map_err(|_| Self::failure("wallet registry schema could not create"))?;
        }
        transaction
            .commit()
            .map_err(|_| Self::failure("wallet registry initialization could not commit"))?;
        Ok(Self {
            connection: Mutex::new(connection),
            admission: Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }

    /// Saves before watch publication. Identical configured imports are harmless on restart.
    pub(crate) async fn keep(
        self: &Arc<Self>,
        id: &str,
        tag: &AssetTag,
        material: SecretBytes,
    ) -> Result<(), WalletError> {
        let id = id.to_owned();
        let tag = tag.to_string();
        let registry = Arc::clone(self);
        let permit = registry
            .admission
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Self::failure("wallet registry unavailable"))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            registry.insert(id, tag, material)
        })
        .await
        .map_err(|_| Self::failure("wallet registry write stopped"))?
    }

    /// One blocking write transaction; a conflicting ID never replaces an existing key.
    fn insert(&self, id: String, tag: String, material: SecretBytes) -> Result<(), WalletError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| Self::failure("wallet registry unavailable"))?;
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|_| Self::failure("wallet registry could not begin write"))?;
        let existing = transaction
            .query_row(
                "SELECT tag, secret FROM wallet WHERE id = ?1",
                [&id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
            .map_err(|_| Self::failure("wallet registry could not read"))?;
        if let Some((prior_tag, prior_secret)) = existing {
            if prior_tag != tag || SecretBytes::new(prior_secret).as_bytes() != material.as_bytes()
            {
                return Err(WalletError::new(
                    WalletErrorKind::Duplicate,
                    "wallet registry ID already belongs to another key or asset",
                ));
            }
        } else {
            transaction
                .execute(
                    "INSERT INTO wallet (id, tag, secret) VALUES (?1, ?2, ?3)",
                    params![id, tag, material.as_bytes()],
                )
                .map_err(|_| Self::failure("wallet registry could not persist key"))?;
        }
        transaction
            .commit()
            .map_err(|_| Self::failure("wallet registry commit outcome is unknown"))?;
        Ok(())
    }

    /// Reconstructs only published wallets. Unobserved keys stay private orphans.
    pub(crate) async fn restore(
        self: &Arc<Self>,
        wallets: &wallets::Wallets<AssetTag>,
        index: &indexing::Index,
        signers: &Signers,
    ) -> Result<(), WalletError> {
        let mut watches = std::collections::BTreeMap::new();
        for network in index.networks() {
            for watch in indexing::Watching::watched(index, network).await? {
                watches.insert(watch.id.clone(), watch);
            }
        }
        for row in self.rows().await? {
            let Some(watch) = watches.get(&row.id) else {
                continue;
            };
            let wallet = wallets.from(&row.tag, row.material).await?;
            if wallet.address() != &watch.address || wallet.asset() != &watch.asset {
                return Err(Self::failure(
                    "wallet registry identity disagrees with its watch",
                ));
            }
            signers.keep(row.id, wallet)?;
        }
        Ok(())
    }

    async fn rows(self: &Arc<Self>) -> Result<Vec<Record>, WalletError> {
        let registry = Arc::clone(self);
        let permit = registry
            .admission
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Self::failure("wallet registry unavailable"))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let connection = registry
                .connection
                .lock()
                .map_err(|_| Self::failure("wallet registry unavailable"))?;
            let mut statement = connection
                .prepare("SELECT id, tag, secret FROM wallet ORDER BY id")
                .map_err(|_| Self::failure("wallet registry could not read"))?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                })
                .map_err(|_| Self::failure("wallet registry could not read"))?;
            rows.map(|row| {
                let (id, tag, secret) =
                    row.map_err(|_| Self::failure("wallet registry row is invalid"))?;
                let material = SecretBytes::new(secret);
                let tag = tag
                    .parse()
                    .map_err(|_| Self::failure("wallet registry asset tag is invalid"))?;
                Ok(Record { id, tag, material })
            })
            .collect()
        })
        .await
        .map_err(|_| Self::failure("wallet registry read stopped"))?
    }

    fn failure(message: &'static str) -> WalletError {
        WalletError::new(WalletErrorKind::Unavailable, message)
    }

    #[cfg(test)]
    pub(crate) async fn ids(self: &Arc<Self>) -> Vec<String> {
        self.rows()
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn fixture() -> Arc<Self> {
        let connection = Connection::open_in_memory().expect("fixture registry");
        connection
            .execute_batch(Self::SCHEMA)
            .expect("fixture schema");
        Arc::new(Self {
            connection: Mutex::new(connection),
            admission: Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn plaintext_rows_survive_reopen_and_conflicting_keys_are_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.db");
        let registry = Registry::open(&path).await.unwrap();
        let tag = "btc".parse().unwrap();
        for _ in 0..2 {
            registry
                .keep("a", &tag, SecretBytes::new([42; 32]))
                .await
                .unwrap();
        }
        assert_eq!(
            registry
                .keep("a", &tag, SecretBytes::new([43; 32]))
                .await
                .unwrap_err()
                .kind,
            WalletErrorKind::Duplicate
        );
        drop(registry);
        let connection = Connection::open(&path).unwrap();
        let bytes: Vec<u8> = connection
            .query_row("SELECT secret FROM wallet WHERE id = 'a'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(bytes, [42; 32]);
        let registry = Registry::open(&path).await.unwrap();
        assert_eq!(
            registry.rows().await.unwrap()[0].material.as_bytes(),
            [42; 32]
        );
    }

    #[tokio::test]
    async fn unsupported_schema_is_refused_without_changing_index_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.db");
        drop(Registry::open(&path).await.unwrap());
        let connection = Connection::open(&path).unwrap();
        for version in [1, 99] {
            connection
                .execute_batch(&format!(
                    "PRAGMA user_version = 23; UPDATE wallet_schema SET version = {version}"
                ))
                .unwrap();
            assert!(Registry::open(&path).await.is_err());
            assert_eq!(
                connection
                    .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
                    .unwrap(),
                23
            );
        }
    }
}
