//! Application-owned encrypted keys, beside indexing in the same SQLite file.

use ring::{
    aead, pbkdf2,
    rand::{SecureRandom, SystemRandom},
};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    num::NonZeroU32,
    path::Path,
    sync::{Arc, Mutex},
};
use wallets::{SecretBytes, WalletError, WalletErrorKind};
use zeroize::Zeroizing;

use crate::api::{AssetTag, Signers};

/// Durable signing material. Neither this value nor its decrypted rows is serializable.
pub(crate) struct Registry {
    connection: Mutex<Connection>,
    admission: Arc<tokio::sync::Semaphore>,
    key: Zeroizing<[u8; 32]>,
}

/// A private boot row; public identity remains owned by the durable watch.
struct Record {
    id: String,
    tag: AssetTag,
    material: SecretBytes,
}

impl Registry {
    // Independent application schema: never changes indexing's user_version.
    const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS wallet_schema (
        id INTEGER PRIMARY KEY CHECK(id = 1), version INTEGER NOT NULL,
        salt BLOB NOT NULL, proof BLOB NOT NULL);
        CREATE TABLE IF NOT EXISTS wallet (
        id TEXT PRIMARY KEY, tag TEXT NOT NULL, secret BLOB NOT NULL) WITHOUT ROWID;";
    // OWASP Password Storage Cheat Sheet, PBKDF2-HMAC-SHA256 work factor.
    const ITERATIONS: NonZeroU32 = NonZeroU32::new(600_000).unwrap();
    const PROOF: &'static [u8] = b"payment-server wallet registry";

    /// Unlocks the app's own tables. The caller already opened the indexing file.
    pub(crate) async fn open(
        path: &Path,
        passphrase: Zeroizing<String>,
    ) -> Result<Arc<Self>, WalletError> {
        if passphrase.is_empty() {
            return Err(Self::failure("wallet passphrase must not be empty"));
        }
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
            Self::from_connection(connection, &passphrase).map(Arc::new)
        })
        .await
        .map_err(|_| Self::failure("wallet registry initialization stopped"))?
    }

    /// Validates the application's schema and authenticates its encryption key atomically.
    fn from_connection(mut connection: Connection, passphrase: &str) -> Result<Self, WalletError> {
        let transaction = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|_| Self::failure("wallet registry could not begin initialization"))?;
        transaction
            .execute_batch(Self::SCHEMA)
            .map_err(|_| Self::failure("wallet registry schema could not open"))?;
        let (salt, proof) = Self::metadata(&transaction)?;
        let mut key = Zeroizing::new([0; 32]);
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            Self::ITERATIONS,
            &salt,
            passphrase.as_bytes(),
            key.as_mut(),
        );
        match proof {
            Some(proof) => {
                let opened = Self::decrypt(&key, Self::PROOF, proof)?;
                if opened.as_bytes() != Self::PROOF {
                    return Err(Self::failure("wallet registry unlock failed"));
                }
            }
            None => {
                let proof = Self::encrypt(&key, Self::PROOF, Self::PROOF)?;
                transaction
                    .execute(
                        "INSERT INTO wallet_schema VALUES (1, 1, ?1, ?2)",
                        params![salt, proof],
                    )
                    .map_err(|_| Self::failure("wallet registry metadata could not persist"))?;
            }
        }
        transaction
            .commit()
            .map_err(|_| Self::failure("wallet registry initialization could not commit"))?;
        Ok(Self {
            connection: Mutex::new(connection),
            admission: Arc::new(tokio::sync::Semaphore::new(1)),
            key,
        })
    }

    fn metadata(
        transaction: &rusqlite::Transaction<'_>,
    ) -> Result<(Vec<u8>, Option<Vec<u8>>), WalletError> {
        let metadata = transaction
            .query_row(
                "SELECT version, salt, proof FROM wallet_schema WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, u32>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| Self::failure("wallet registry metadata is invalid"))?;
        match metadata {
            Some((1, salt, proof)) if salt.len() == 32 => Ok((salt, Some(proof))),
            Some(_) => Err(Self::failure("unsupported wallet registry schema")),
            None => {
                let count: i64 = transaction
                    .query_row("SELECT COUNT(*) FROM wallet", [], |row| row.get(0))
                    .map_err(|_| Self::failure("wallet registry could not read"))?;
                if count != 0 {
                    return Err(Self::failure("wallet registry metadata is missing"));
                }
                let mut salt = vec![0; 32];
                SystemRandom::new()
                    .fill(&mut salt)
                    .map_err(|_| Self::failure("wallet registry randomness unavailable"))?;
                Ok((salt, None))
            }
        }
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
        let aad = serde_json::to_vec(&(&id, &tag))
            .map_err(|_| Self::failure("wallet identity could not encode"))?;
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
            if prior_tag != tag
                || Self::decrypt(&self.key, &aad, prior_secret)?.as_bytes() != material.as_bytes()
            {
                return Err(WalletError::new(
                    WalletErrorKind::Duplicate,
                    "wallet registry ID already belongs to another key or asset",
                ));
            }
        } else {
            let secret = Self::encrypt(&self.key, &aad, material.as_bytes())?;
            transaction
                .execute(
                    "INSERT INTO wallet (id, tag, secret) VALUES (?1, ?2, ?3)",
                    params![id, tag, secret],
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
                let (id, tag, encrypted) =
                    row.map_err(|_| Self::failure("wallet registry row is invalid"))?;
                let aad = serde_json::to_vec(&(&id, &tag))
                    .map_err(|_| Self::failure("wallet identity could not encode"))?;
                let material = Self::decrypt(&registry.key, &aad, encrypted)?;
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

    fn encrypt(key: &[u8; 32], aad: &[u8], material: &[u8]) -> Result<Vec<u8>, WalletError> {
        let key = aead::UnboundKey::new(&aead::AES_256_GCM, key)
            .map_err(|_| Self::failure("wallet encryption unavailable"))?;
        let mut nonce = [0; aead::NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| Self::failure("wallet registry randomness unavailable"))?;
        let mut secret = Zeroizing::new(material.to_vec());
        aead::LessSafeKey::new(key)
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(aad),
                &mut *secret,
            )
            .map_err(|_| Self::failure("wallet encryption failed"))?;
        let mut encoded = nonce.to_vec();
        encoded.extend_from_slice(&secret);
        Ok(encoded)
    }

    fn decrypt(
        key: &[u8; 32],
        aad: &[u8],
        mut encrypted: Vec<u8>,
    ) -> Result<SecretBytes, WalletError> {
        if encrypted.len() < aead::NONCE_LEN {
            return Err(Self::failure("wallet registry ciphertext is invalid"));
        }
        let mut nonce = [0; aead::NONCE_LEN];
        nonce.copy_from_slice(&encrypted[..aead::NONCE_LEN]);
        let key = aead::UnboundKey::new(&aead::AES_256_GCM, key)
            .map_err(|_| Self::failure("wallet decryption unavailable"))?;
        let mut encoded = Zeroizing::new(encrypted.split_off(aead::NONCE_LEN));
        let plaintext = aead::LessSafeKey::new(key)
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(aad),
                &mut encoded,
            )
            .map_err(|_| {
                Self::failure("wallet registry unlock failed or ciphertext was modified")
            })?;
        Ok(SecretBytes::new(plaintext))
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
            key: Zeroizing::new([7; 32]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn encrypted_rows_survive_reopen_and_reject_substitution_and_wrong_password() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.db");
        let password = || Zeroizing::new("offline fixture passphrase".to_owned());
        let registry = Registry::open(&path, password()).await.unwrap();
        let tag = "btc".parse().unwrap();
        registry
            .keep("a", &tag, SecretBytes::new([42; 32]))
            .await
            .unwrap();
        registry
            .keep("a", &tag, SecretBytes::new([42; 32]))
            .await
            .unwrap();
        assert_eq!(
            registry
                .keep("a", &tag, SecretBytes::new([43; 32]))
                .await
                .unwrap_err()
                .kind,
            WalletErrorKind::Duplicate
        );
        drop(registry);
        let raw = std::fs::read(&path).unwrap();
        assert!(!raw.windows(32).any(|window| window == [42; 32]));
        assert!(
            !raw.windows(64)
                .any(|window| window == hex::encode([42; 32]).as_bytes())
        );
        assert!(
            Registry::open(&path, Zeroizing::new("wrong".into()))
                .await
                .is_err()
        );
        let registry = Registry::open(&path, password()).await.unwrap();
        let rows = registry.rows().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].material.as_bytes(), [42; 32]);
        registry
            .connection
            .lock()
            .unwrap()
            .execute("UPDATE wallet SET id = 'b'", [])
            .unwrap();
        assert!(
            registry.rows().await.is_err(),
            "ciphertext authenticates its identity"
        );
    }

    #[test]
    fn cipher_rejects_modification_and_uses_distinct_nonces() {
        let first = Registry::encrypt(&[1; 32], b"identity", &[42; 32]).unwrap();
        let mut second = Registry::encrypt(&[1; 32], b"identity", &[42; 32]).unwrap();
        assert_ne!(first, second);
        second[15] ^= 1;
        assert!(Registry::decrypt(&[1; 32], b"identity", second).is_err());
        assert!(Registry::decrypt(&[1; 32], b"another identity", first).is_err());
    }

    #[tokio::test]
    async fn unknown_application_schema_is_refused_without_changing_index_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.db");
        let registry = Registry::open(&path, Zeroizing::new("fixture passphrase".into()))
            .await
            .unwrap();
        {
            let connection = registry.connection.lock().unwrap();
            connection
                .execute_batch("PRAGMA user_version = 23; UPDATE wallet_schema SET version = 99")
                .unwrap();
        }
        drop(registry);
        assert!(
            Registry::open(&path, Zeroizing::new("fixture passphrase".into()))
                .await
                .is_err()
        );
        let connection = Connection::open(path).unwrap();
        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
                .unwrap(),
            23
        );
    }
    #[tokio::test]
    async fn empty_passphrase_never_creates_a_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registry.db");
        assert!(
            Registry::open(&path, Zeroizing::new(String::new()))
                .await
                .is_err()
        );
        assert!(!path.exists());
    }
}
