//! Server-owned configuration in one table; indexing owns its own schema/version.

use crate::{config::AnyError, lock::Lock as _};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

mod chain;
mod command;

pub(crate) use chain::{Chain, Network, Node, Phase, ReindexRequest, Requested};
pub(crate) use command::Action;

#[derive(Clone)]
pub(crate) struct ChainRecord {
    pub(crate) revision: u64,
    pub(crate) chain: Chain,
}

pub(crate) struct Settings {
    database: Mutex<Connection>,
    rows: Mutex<BTreeMap<String, ChainRecord>>,
    /// Every configured network, derived once per committed change and
    /// published to whoever follows the table.
    changed: watch::Sender<Arc<Vec<base::Network>>>,
}

impl Settings {
    pub(crate) fn seed(
        sections: sdk_config::Sections,
    ) -> Result<BTreeMap<String, Chain>, AnyError> {
        sections
            .into_values()
            .into_iter()
            .map(|(id, mut section)| {
                crate::chains::explicit_asset(&id, &mut section);
                Chain::seed(section).map(|chain| (id, chain))
            })
            .collect()
    }

    pub(crate) async fn open(
        path: &Path,
        seed: BTreeMap<String, Chain>,
    ) -> Result<Arc<Self>, AnyError> {
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut database = Connection::open(path)?;
            database.busy_timeout(std::time::Duration::from_secs(5))?;
            database.execute_batch("PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;")?;
            let transaction =
                database.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            Self::migrate(&transaction, seed)?;
            let rows = Self::read(&transaction)?;
            Self::unique(&rows)?;
            transaction.commit()?;
            let networks = Self::derive(&rows);
            Ok(Arc::new(Self {
                database: Mutex::new(database),
                rows: Mutex::new(rows),
                changed: watch::channel(networks).0,
            }))
        })
        .await?
    }

    fn migrate(
        transaction: &rusqlite::Transaction<'_>,
        seed: BTreeMap<String, Chain>,
    ) -> Result<(), AnyError> {
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='chains')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            transaction.execute_batch(
                "CREATE TABLE chains (
                    id TEXT PRIMARY KEY, config TEXT NOT NULL,
                    revision INTEGER NOT NULL CHECK(revision >= 0),
                    version INTEGER NOT NULL CHECK(version > 0)) WITHOUT ROWID;",
            )?;
            for (id, chain) in seed {
                chain.validate(&id)?;
                transaction.execute(
                    "INSERT INTO chains VALUES (?1, ?2, 0, 1)",
                    params![id, serde_json::to_string(&chain)?],
                )?;
            }
        }
        Ok(())
    }

    fn read(transaction: &Connection) -> Result<BTreeMap<String, ChainRecord>, AnyError> {
        let mut statement =
            transaction.prepare("SELECT id, config, revision, version FROM chains")?;
        let records = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        let mut rows = BTreeMap::new();
        for record in records {
            let (id, document, revision, version) = record?;
            if version != 1 {
                return Err("unsupported chains schema version".into());
            }
            let chain: Chain =
                serde_json::from_str(&document).map_err(|_| "invalid saved chain settings")?;
            chain.validate(&id)?;
            let revision = u64::try_from(revision).map_err(|_| "invalid settings revision")?;
            rows.insert(id, ChainRecord { revision, chain });
        }
        Ok(rows)
    }

    pub(crate) fn all(&self) -> BTreeMap<String, ChainRecord> {
        self.rows.hold().clone()
    }

    /// Every configured network, as of the last committed change.
    pub(crate) fn networks(&self) -> Arc<Vec<base::Network>> {
        self.changed.borrow().clone()
    }

    /// Follows the table: changes whenever a commit lands, carrying the
    /// networks it configures.
    pub(crate) fn changes(&self) -> watch::Receiver<Arc<Vec<base::Network>>> {
        self.changed.subscribe()
    }

    /// One network's saved row, with the revision of the chain it belongs to.
    pub(crate) fn desired(&self, network: &base::Network) -> Option<(u64, Network)> {
        let rows = self.rows.hold();
        let row = rows.get(&network.chain.to_string())?;
        let saved = row.chain.networks.iter().find(|n| n.name == network.name)?;
        Some((row.revision, saved.clone()))
    }

    /// Whether `revision` is still the saved revision of `network`'s chain.
    pub(crate) fn current(&self, network: &base::Network, revision: u64) -> bool {
        self.rows
            .hold()
            .get(&network.chain.to_string())
            .map(|row| row.revision)
            == Some(revision)
    }

    fn derive(rows: &BTreeMap<String, ChainRecord>) -> Arc<Vec<base::Network>> {
        Arc::new(
            rows.iter()
                .flat_map(|(id, row)| {
                    row.chain.networks.iter().map(move |network| {
                        base::Network::new(base::ChainName::new(id.clone()), network.name.clone())
                    })
                })
                .collect(),
        )
    }

    pub(crate) async fn save(
        self: &Arc<Self>,
        id: String,
        expected: Option<u64>,
        chain: Chain,
    ) -> Result<u64, AnyError> {
        chain.validate(&id)?;
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.commit(id, expected, chain)).await?
    }

    // Durable write and cache publication finish even if the awaiting request is cancelled.
    fn commit(&self, id: String, expected: Option<u64>, chain: Chain) -> Result<u64, AnyError> {
        let mut database = self.database.hold();
        let transaction =
            database.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: Option<i64> = transaction
            .query_row("SELECT revision FROM chains WHERE id=?1", [&id], |row| {
                row.get(0)
            })
            .optional()?;
        let current = current
            .map(u64::try_from)
            .transpose()
            .map_err(|_| "invalid settings revision")?;
        if current != expected {
            return Err("configuration revision conflict".into());
        }
        let revision = current
            .map_or(Some(0), |v| v.checked_add(1))
            .filter(|v| *v <= i64::MAX as u64)
            .ok_or("configuration revision exhausted")?;
        let mut rows = self.rows.hold();
        let mut next = rows.clone();
        next.insert(
            id.clone(),
            ChainRecord {
                revision,
                chain: chain.clone(),
            },
        );
        Self::unique(&next)?;
        transaction.execute(
            "INSERT INTO chains VALUES (?1, ?2, ?3, 1)
                ON CONFLICT(id) DO UPDATE SET config=excluded.config, revision=excluded.revision",
            params![id, serde_json::to_string(&chain)?, i64::try_from(revision)?],
        )?;
        transaction.commit()?;
        let networks = Self::derive(&next);
        *rows = next;
        self.changed.send_replace(networks);
        Ok(revision)
    }

    fn unique(rows: &BTreeMap<String, ChainRecord>) -> Result<(), AnyError> {
        let mut tags = BTreeSet::new();
        for network in rows.values().flat_map(|row| &row.chain.networks) {
            for tag in network.tags()? {
                if !tags.insert(tag) {
                    return Err("asset tags must be unique across networks".into());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chain() -> Chain {
        Chain::seed(json!({
            "network": "mainnet", "chain_id": 1, "asset": "eth",
            "rpc": {"endpoints": [{"url": "http://127.0.0.1:8545",
                "headers": {"Authorization": "plain-test-credential"}}]}
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn settings_seed_once_and_preserve_other_schema_owners() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("payment.db");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "PRAGMA user_version=41;
            CREATE TABLE wallet_schema(version INTEGER); INSERT INTO wallet_schema VALUES(2);
            CREATE TABLE application_data(value TEXT);
            INSERT INTO application_data VALUES('keep');",
        )
        .unwrap();
        let settings = Settings::open(&path, BTreeMap::from([("ethereum".into(), chain())]))
            .await
            .unwrap();
        let mut saved = settings.all().remove("ethereum").unwrap();
        saved.chain.networks[0].indexing = Requested::Paused;
        assert_eq!(
            settings
                .save("ethereum".into(), Some(0), saved.chain)
                .await
                .unwrap(),
            1
        );
        assert!(
            settings
                .save("ethereum".into(), Some(0), chain())
                .await
                .is_err()
        );
        drop(settings);
        let settings = Settings::open(&path, BTreeMap::new()).await.unwrap();
        let saved = settings.all().remove("ethereum").unwrap();
        assert_eq!(saved.revision, 1);
        assert!(saved.chain.networks[0].indexing == Requested::Paused);
        let document: String = db
            .query_row("SELECT config FROM chains", [], |r| r.get(0))
            .unwrap();
        assert!(document.contains("plain-test-credential"));
        assert_eq!(
            db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            41
        );
        assert_eq!(
            db.query_row("SELECT version FROM wallet_schema", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            db.query_row("SELECT value FROM application_data", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "keep"
        );
        drop(settings);
        db.execute("DELETE FROM chains", []).unwrap();
        let empty = Settings::open(&path, BTreeMap::from([("ethereum".into(), chain())]))
            .await
            .unwrap();
        assert!(
            empty.all().is_empty(),
            "an existing empty table never reseeds"
        );
    }

    #[tokio::test]
    async fn invalid_seed_rolls_back_and_unknown_version_stays_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("payment.db");
        let mut invalid = chain();
        invalid.networks[0].name = "wrong".into();
        assert!(
            Settings::open(&path, BTreeMap::from([("ethereum".into(), invalid)]))
                .await
                .is_err()
        );
        let db = Connection::open(&path).unwrap();
        let exists: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='chains')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!exists);
        let settings = Settings::open(&path, BTreeMap::from([("ethereum".into(), chain())]))
            .await
            .unwrap();
        drop(settings);
        db.execute("UPDATE chains SET version=99", []).unwrap();
        assert!(Settings::open(&path, BTreeMap::new()).await.is_err());
        assert_eq!(
            db.query_row("SELECT version FROM chains", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            99
        );
    }

    fn network() -> base::Network {
        base::Network::new(base::ChainName::new("ethereum"), "mainnet")
    }

    /// `desired` is one network's row; commands are read-modify-write of it,
    /// and each commit publishes the derived network list to followers.
    #[tokio::test]
    async fn commands_amend_one_row_and_publish_the_networks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("payment.db");
        let settings = Settings::open(&path, BTreeMap::from([("ethereum".into(), chain())]))
            .await
            .unwrap();
        let changes = settings.changes();
        assert_eq!(*settings.networks(), [network()]);
        let (revision, row) = settings.desired(&network()).unwrap();
        assert_eq!((revision, row.name.as_str()), (0, "mainnet"));
        let other = base::Network::new(base::ChainName::new("ethereum"), "sepolia");
        assert!(settings.desired(&other).is_none());

        settings.command(&network(), Action::Pause).await.unwrap();
        assert!(changes.has_changed().unwrap());
        assert!(settings.desired(&network()).unwrap().1.indexing == Requested::Paused);
        settings.command(&network(), Action::Resume).await.unwrap();
        let (revision, row) = settings.desired(&network()).unwrap();
        assert_eq!((revision, row.resume), (2, 1));
        assert!(settings.current(&network(), 2) && !settings.current(&network(), 1));

        settings.command(&network(), Action::Reindex).await.unwrap();
        assert!(settings.command(&network(), Action::Reindex).await.is_err());
        let id = settings.desired(&network()).unwrap().1.rebuild.unwrap().id;
        assert!(settings.rebuild(&network(), "another", None).await.is_err());
        settings.rebuild(&network(), &id, None).await.unwrap();
        assert!(settings.desired(&network()).unwrap().1.rebuild.is_none());
        assert!(settings.command(&other, Action::Pause).await.is_err());
    }

    /// A write against a revision that has since moved is refused whole.
    #[tokio::test]
    async fn a_stale_revision_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("payment.db");
        let settings = Settings::open(&path, BTreeMap::from([("ethereum".into(), chain())]))
            .await
            .unwrap();
        let stale = settings.all().remove("ethereum").unwrap();
        settings.command(&network(), Action::Pause).await.unwrap();
        assert!(
            settings
                .save("ethereum".into(), Some(stale.revision), stale.chain)
                .await
                .is_err()
        );
        assert!(settings.desired(&network()).unwrap().1.indexing == Requested::Paused);
    }
}
