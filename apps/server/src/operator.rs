//! Offline operator actions. Stop the server first; these never connect to RPC,
//! start the supervisor or bind a listener. Application keys are never purged.

use crate::config::{AnyError, Config};
use base::{ChainName, Network};
use indexing::{IndexStore as _, Purging as _, WatchStore as _};
use std::{ffi::OsString, fmt};

/// One explicit operator action over the configured file.
pub(crate) enum Operation {
    Rescan(Network),
    CancelBackfill(Network),
}

impl Operation {
    pub(crate) fn parse(
        arguments: impl Iterator<Item = OsString>,
    ) -> Result<Option<Self>, AnyError> {
        let arguments = arguments.collect::<Vec<_>>();
        match arguments.as_slice() {
            [] => Ok(None),
            [flag, target] if flag == "--rescan" || flag == "--cancel-backfill" => {
                let text = target.to_str().ok_or("network argument is not UTF-8")?;
                let network = Self::network(text)?;
                Ok(Some(if flag == "--rescan" {
                    Self::Rescan(network)
                } else {
                    Self::CancelBackfill(network)
                }))
            }
            _ => Err(crate::USAGE.into()),
        }
    }

    fn network(text: &str) -> Result<Network, AnyError> {
        let malformed = || format!("operator command takes one <chain>/<network> argument: {text}");
        let (chain, name) = text.split_once('/').ok_or_else(malformed)?;
        if chain.trim().is_empty() || name.trim().is_empty() || name.contains('/') {
            return Err(malformed().into());
        }
        Ok(Network::new(ChainName::new(chain), name))
    }

    pub(crate) async fn run(&self, config: &Config) -> Result<(), AnyError> {
        let configured = config.chains()?.networks();
        let (Self::Rescan(network) | Self::CancelBackfill(network)) = self;
        if !configured.contains(network) {
            let listed = configured
                .iter()
                .map(|network| format!("{}/{}", network.chain, network.name))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "{self} is not a configured network; this file configures {listed}"
            )
            .into());
        }
        let store = config.storage.open().await?;
        let repository = store.repository();
        match self {
            Self::Rescan(network) => {
                let removed = repository.purge_network(network).await?;
                println!(
                    "removed {} checkpoint, {} history, {} movement, \
                     {} block-credit, and {} output rows \
                     from {self}; watches and application keys preserved",
                    removed.checkpoint,
                    removed.history,
                    removed.movements,
                    removed.credits,
                    removed.outputs
                );
            }
            Self::CancelBackfill(network) => {
                let count = Self::cancel(&repository, network).await?;
                println!(
                    "cancelled backfill for {count} watches on {self}; \
                     indexed facts and application keys preserved"
                );
            }
        }
        Ok(())
    }

    /// Re-anchors only genuinely unfinished watches, using the same durable
    /// progress predicate as the scanner; there is no running pass to coordinate.
    async fn cancel(
        repository: &crate::storage::Repository,
        network: &Network,
    ) -> Result<usize, AnyError> {
        let Some(checkpoint) = repository.checkpoint(network).await? else {
            return Ok(0);
        };
        let rows = repository.all(network).await?;
        let owing = rows
            .into_iter()
            .filter(|progress| !progress.caught_up(Some(&checkpoint)))
            .collect::<Vec<_>>();
        if owing.is_empty() {
            return Ok(0);
        }
        let birthday = checkpoint
            .position
            .checked_successor()
            .ok_or("checkpoint has no successor")?;
        let renewed = owing
            .into_iter()
            .map(|progress| indexing::Watch {
                start: birthday,
                ..progress.watch
            })
            .collect::<Vec<_>>();
        let count = renewed.len();
        repository.cancel_backfill(renewed).await?;
        Ok(count)
    }
}

impl fmt::Display for Operation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rescan(network) | Self::CancelBackfill(network) => {
                write!(formatter, "{}/{}", network.chain, network.name)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, path::Path};

    use indexing::{
        Address, AssetId, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef,
        CanonicalBlock, Decimal, IndexStore as _, IndexedBatch, InterpretedBlock, MovementId,
        ObservationDraft, ObservationDraftStatus, TransactionRef, ValueMovement, Watch,
        WatchStore as _,
    };
    use serde_json::{Value, json};

    use super::*;

    const GENESIS: &str = "11111111111111111111111111111111";

    fn arguments<const N: usize>(values: [&str; N]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn parse<const N: usize>(values: [&str; N]) -> Result<Option<Operation>, AnyError> {
        Operation::parse(arguments(values).into_iter())
    }

    /// A configuration whose only chain is Solana, on one index file inside
    /// `directory`, bound to `port`.
    fn value(directory: &Path, port: u16) -> Value {
        json!({
            "bind": format!("127.0.0.1:{port}"),
            "bearer_token_env": "PAYMENT_RESCAN_UNSET_TOKEN",
            "storage": {"path": directory.join("payment.db")},
            "chains": {"solana": {
                "network": "localnet",
                "genesis_hash": GENESIS,
                "rpc": {"endpoints": ["http://127.0.0.1:8899"], "repeat": "off"},
                "cadence": {"poll_millis": 1000, "batch_size": 100}
            }}
        })
    }

    async fn written(directory: &Path, port: u16) -> Config {
        let path = directory.join("config.json");
        tokio::fs::write(&path, value(directory, port).to_string())
            .await
            .expect("configuration file");
        Config::read(path).await.expect("valid configuration")
    }

    /// One committed block crediting `owner`: a checkpoint and one history
    /// entry, so the reported counts cannot all be zero by accident.
    fn indexed(network: &Network) -> IndexedBatch {
        let interpreted = InterpretedBlock::new(
            network,
            BlockRef {
                position: BlockPosition(1),
                height: BlockHeight(1),
                hash: BlockHash(vec![1]),
                parent: Some(BlockParent {
                    position: BlockPosition(0),
                    hash: BlockHash(vec![0]),
                }),
                timestamp: Some(1_000),
            },
            vec![ObservationDraft {
                transaction: indexing::Transaction {
                    transaction_id: TransactionRef::new(network.clone(), "credit"),
                    movements: vec![ValueMovement::mint(
                        MovementId("0".into()),
                        AssetId::native(network.chain.clone()),
                        Decimal::from(5_u64),
                        Address::new(network.clone(), "owner"),
                    )],
                    fee: None,
                },
                transaction_order: 0,
                status: ObservationDraftStatus::Included,
            }],
        )
        .expect("unique transaction ids");
        let facts = CanonicalBlock::new(network.clone(), interpreted).expect("valid fixture facts");
        IndexedBatch::new(
            network.clone(),
            None,
            vec![facts],
            Vec::new(),
            indexing::FeeSettlement::Charged,
        )
        .expect("valid fixture block")
    }

    fn registration(network: &Network) -> Watch {
        Watch {
            id: format!("{}-{}-owner", network.chain, network.name),
            address: Address::new(network.clone(), "owner"),
            asset: AssetId::native(network.chain.clone()),
            start: BlockPosition(4),
        }
    }

    #[test]
    fn an_absent_flag_is_an_ordinary_start_and_every_other_spelling_is_refused() {
        assert!(parse([]).expect("no subcommand").is_none());
        for invalid in [
            vec!["--rescan"],
            vec!["--rescan", "solana/localnet", "extra"],
            vec!["rescan", "solana/localnet"],
            vec!["--purge", "solana/localnet"],
            vec!["solana/localnet"],
        ] {
            Operation::parse(invalid.iter().map(OsString::from))
                .err()
                .unwrap_or_else(|| panic!("{invalid:?} must be refused"));
        }
    }

    #[test]
    fn a_malformed_target_is_refused_before_any_configuration_is_read() {
        for target in ["bitcoin", "", "/mainnet", "bitcoin/", " /mainnet", "a/b/c"] {
            let error = parse(["--rescan", target])
                .err()
                .unwrap_or_else(|| panic!("{target} must be refused"));
            assert!(
                error.to_string().starts_with("operator command takes one"),
                "{error}"
            );
        }
        let rescan = parse(["--rescan", "solana/localnet"])
            .expect("a well-formed target")
            .expect("a rescan");
        assert_eq!(rescan.to_string(), "solana/localnet");
    }

    #[tokio::test]
    async fn a_network_the_file_does_not_configure_is_refused_and_removes_nothing() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = written(directory.path(), 0).await;
        let refused = parse(["--rescan", "bitcoin/mainnet"])
            .expect("a well-formed target")
            .expect("a rescan");

        let error = refused
            .run(&config)
            .await
            .expect_err("an unconfigured network must be refused");

        assert_eq!(
            error.to_string(),
            "bitcoin/mainnet is not a configured network; \
             this file configures solana/localnet"
        );
        assert!(
            !directory.path().join("payment.db").exists(),
            "a refused rescan must not even open storage"
        );
    }

    /// The whole tool: it empties the named network, leaves every other
    /// network and the address registry alone, reports the counts, and never
    /// binds the configured address — which the listener this test holds open
    /// on that address proves.
    #[tokio::test]
    async fn it_purges_only_the_named_network_and_never_binds_the_listener() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let held = TcpListener::bind("127.0.0.1:0").expect("a held address");
        let port = held.local_addr().expect("bound address").port();
        let config = written(directory.path(), port).await;
        let target = Network::new(ChainName::new("solana"), "localnet");
        let other = Network::new(ChainName::new("bitcoin"), "regtest");
        let store = config.storage.open().await.expect("storage");
        let repository = store.repository();
        for network in [&target, &other] {
            repository
                .add(vec![registration(network)])
                .await
                .expect("the observation");
            repository
                .commit(indexed(network))
                .await
                .expect("one committed block");
        }
        drop(store);

        parse(["--rescan", "solana/localnet"])
            .expect("a well-formed target")
            .expect("a rescan")
            .run(&config)
            .await
            .expect("the configured network is purged");

        // What the store removed is `indexing`'s own contract, asserted count
        // by count in its own `PurgeContract`. This tool's subject
        // is which network it names and what it leaves alone.
        let store = config.storage.open().await.expect("reopened storage");
        let repository = store.repository();
        for network in [&target, &other] {
            assert_eq!(
                repository
                    .all(network)
                    .await
                    .expect("observations")
                    .into_iter()
                    .map(|observation| observation.watch)
                    .collect::<Vec<_>>(),
                [registration(network)],
                "an offline rescan never touches what the index observes"
            );
            let purged = network == &target;
            assert_eq!(
                repository.checkpoint(network).await.expect("tip").is_none(),
                purged,
                "only {network:?} was named on the command line"
            );
        }
        drop(held);
    }
    #[tokio::test]
    async fn cancellation_preserves_other_networks_facts_and_keys() {
        let directory = tempfile::tempdir().expect("directory");
        let held = TcpListener::bind("127.0.0.1:0").expect("occupied listener");
        let config = written(directory.path(), held.local_addr().expect("address").port()).await;
        let target = Network::new(ChainName::new("solana"), "localnet");
        let other = Network::new(ChainName::new("bitcoin"), "regtest");
        let store = config.storage.open().await.expect("store");
        let repository = store.repository();
        for network in [&target, &other] {
            repository
                .commit(indexed(network))
                .await
                .expect("checkpoint");
            let mut watch = registration(network);
            watch.start = BlockPosition(0);
            repository.add(vec![watch]).await.expect("late watch");
        }
        let sentinel =
            rusqlite::Connection::open(directory.path().join("payment.db")).expect("connection");
        sentinel
            .execute_batch(
                "CREATE TABLE app_sentinel(secret TEXT);
             INSERT INTO app_sentinel VALUES ('untouched')",
            )
            .expect("application row");
        let before = repository.all(&other).await.expect("other watches");
        parse(["--cancel-backfill", "solana/localnet"])
            .expect("command")
            .expect("operation")
            .run(&config)
            .await
            .expect("cancelled without binding or RPC");
        assert_eq!(repository.all(&other).await.expect("other"), before);
        let target_rows = repository.all(&target).await.expect("target");
        assert_eq!(target_rows[0].watch.start, BlockPosition(2));
        assert_eq!(
            Operation::cancel(&repository, &target)
                .await
                .expect("idempotent"),
            0
        );
        for network in [&target, &other] {
            assert_eq!(
                repository
                    .checkpoint(network)
                    .await
                    .expect("checkpoint")
                    .expect("retained")
                    .position,
                BlockPosition(1)
            );
        }
        assert_eq!(
            sentinel
                .query_row("SELECT secret FROM app_sentinel", [], |row| row
                    .get::<_, String>(0))
                .expect("application row"),
            "untouched"
        );
    }
}
