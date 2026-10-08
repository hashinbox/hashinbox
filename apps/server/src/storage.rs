//! The one SQLite file: opened once as one pool, its indexing schema handed
//! to every chain as one [`Repository`] — which also holds every wallet
//! (`indexing::WalletRegistry`) — and the server's own table
//! (`models/schema.sql`) created beside it, in the same file and under the
//! same identity, for the models (`crate::models`).

use std::{path, path::PathBuf};

use serde::Deserialize;

use crate::config::AnyError;

/// The one repository every chain composes over: it implements the two ports an
/// index requires and every consumer capability this server's chains read.
pub(crate) type Repository = indexing::sqlite::Repository;

/// The `"storage"` configuration: `{"path": ...}`, defaulting to `payment.db`
/// in the working directory.
///
/// There is one store, so this names a file rather than choosing a backend. A
/// path may be written relative: this is the composition root, so it resolves
/// the path against the working directory before the backend, which only
/// accepts an absolute path, ever sees it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            path: PathBuf::from("payment.db"),
        }
    }
}

impl Config {
    pub(crate) fn validate(&self) -> Result<(), AnyError> {
        self.resolved_path().map(drop)
    }

    /// Opens the configured file as one pool: an empty file is given
    /// indexing's schema and the server's (`crate::models::SCHEMA`), a file
    /// holding exactly those is opened as it is, and any other is refused.
    pub(crate) async fn open(&self) -> Result<Store, AnyError> {
        let path = self.resolved_path()?;
        let database = indexing::sqlite::Database::open_with(path, crate::models::SCHEMA).await?;
        Ok(Store {
            pool: database.pool().clone(),
            repository: indexing::sqlite::Repository::new(database),
        })
    }

    /// The database file as the backend requires it: absolute, resolved
    /// against the working directory. Validation runs it so a path that cannot
    /// be resolved fails at configuration time, before any chain connects.
    pub(crate) fn resolved_path(&self) -> Result<PathBuf, AnyError> {
        if self.path.as_os_str().is_empty() {
            return Err("storage path must not be empty".into());
        }
        path::absolute(&self.path).map_err(|error| {
            format!("storage path is not resolvable against the working directory: {error}").into()
        })
    }
}

/// The opened database file, shared by every network and every model.
pub(crate) struct Store {
    pool: sqlx::SqlitePool,
    repository: Repository,
}

impl Store {
    /// The repository over this file. One value serves every network: each
    /// operation names the network it is about.
    pub(crate) fn repository(&self) -> Repository {
        self.repository.clone()
    }

    /// The file's one pool, for the models' stores.
    pub(crate) fn pool(&self) -> sqlx::SqlitePool {
        self.pool.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::env;

    use indexing::{IndexStore as _, Network, WatchStore as _};

    use super::*;

    /// One file, named by a path and nothing else: a document that still names
    /// a backend is refused rather than quietly ignored.
    #[test]
    fn storage_names_a_file_and_never_a_backend() {
        assert!(Config::default().path.ends_with("payment.db"));
        let configured: Config = serde_json::from_value(serde_json::json!({"path": "/tmp/x.db"}))
            .expect("sqlite storage");
        configured.validate().expect("valid storage");
        for invalid in [
            serde_json::json!({"path": "a", "extra": 1}),
            serde_json::json!({"redb": {"path": "a"}}),
            serde_json::json!({"sqlite": {"path": "a"}}),
            serde_json::json!({"postgres": {"url_env": "URL"}}),
        ] {
            assert!(serde_json::from_value::<Config>(invalid).is_err());
        }
        let empty: Config =
            serde_json::from_value(serde_json::json!({"path": ""})).expect("syntactically valid");
        assert!(empty.validate().is_err());
    }

    #[tokio::test]
    async fn a_relative_path_opens_against_the_working_directory() {
        let working = env::current_dir().expect("working directory");
        let default = Config::default();
        let resolved = default.resolved_path().expect("the default path resolves");
        assert_eq!(resolved, working.join("payment.db"));
        default.validate().expect("the default storage validates");

        let directory = tempfile::Builder::new()
            .prefix("relative-storage")
            .tempdir_in(&working)
            .expect("temporary directory beside the working directory");
        let relative = PathBuf::from(
            directory
                .path()
                .file_name()
                .expect("temporary directory name"),
        )
        .join("server.db");
        assert!(!relative.is_absolute());
        let config: Config =
            serde_json::from_value(serde_json::json!({"path": relative})).expect("sqlite storage");
        config.validate().expect("a relative path validates");
        config.open().await.expect("a relative path opens");
    }

    #[tokio::test]
    async fn one_file_serves_every_network() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config: Config = serde_json::from_value(serde_json::json!({
            "path": directory.path().join("server.db")
        }))
        .expect("sqlite storage");
        let store = config.open().await.expect("open the index");
        let repository = store.repository();
        for chain in ["one", "two"] {
            let network = Network::new(indexing::ChainName::new(chain), "net");
            assert_eq!(repository.checkpoint(&network).await.expect("tip"), None);
            assert!(
                repository
                    .all(&network)
                    .await
                    .expect("observations")
                    .is_empty()
            );
        }
    }
    /// Indexing, wallets and endpoints share one file and one pool: each
    /// survives a restart, and a network purge leaves the server's table and
    /// every wallet's label and key alone.
    #[tokio::test]
    async fn one_pool_serves_indexing_and_every_model_across_a_restart() {
        use crate::models::RpcEndpoints;
        use indexing::{
            Address, AssetId, BlockPosition, IndexStore as _, Registration, WalletRegistry as _,
            Watch,
        };

        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("server.db");
        let config: Config =
            serde_json::from_value(serde_json::json!({"path": path})).expect("sqlite storage");
        let network = Network::new(indexing::ChainName::new("bitcoin"), "regtest");
        let endpoints = RpcEndpoints::new(vec![serde_json::json!("http://node.invalid:8332")])
            .expect("an endpoint");
        let watch = Watch {
            id: "saved-key".to_owned(),
            address: Address::new(network.clone(), "addr"),
            asset: AssetId::native(network.chain.clone()),
            start: BlockPosition(3),
        };
        {
            let store = config.open().await.expect("opened");
            store
                .repository()
                .register(Registration {
                    id: "saved-key".to_owned(),
                    address: Address::new(network.clone(), "addr"),
                    asset: AssetId::native(network.chain.clone()),
                    label: Some("cold".to_owned()),
                    secret: Some(wallets::SecretBytes::new([42; 32])),
                })
                .await
                .expect("filed");
            RpcEndpoints::insert_missing(&store.pool(), &network, &endpoints)
                .await
                .expect("seeded");
            let repository = store.repository();
            repository.add(vec![watch.clone()]).await.expect("watched");
            repository.purge(&network).await.expect("purged");
        }

        let store = config.open().await.expect("reopened");
        let wallets = store.repository().wallets().await.expect("readable");
        assert_eq!(wallets.len(), 1);
        assert_eq!(wallets[0].label(), Some("cold"), "a purge keeps the label");
        assert_eq!(
            wallets[0].secret().map(|key| key.as_bytes().to_vec()),
            Some(vec![42; 32]),
            "and the key"
        );
        let read = RpcEndpoints::find_all(&store.pool())
            .await
            .expect("stored endpoints");
        let documents: Vec<_> = read.iter().map(|(_, stored)| stored.documents()).collect();
        assert_eq!(documents, [endpoints.documents()]);
        let watched = store.repository().all(&network).await.expect("watches");
        assert_eq!(
            watched.iter().map(|row| &row.watch).collect::<Vec<_>>(),
            [&watch]
        );
    }
}
