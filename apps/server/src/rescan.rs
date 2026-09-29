//! The offline rescan: `payment-server <config.toml> --rescan <chain>/<network>`.
//!
//! One network's indexing rows are removed and the process exits. Nothing else
//! happens: no node is contacted, no supervisor starts, and the HTTP listener
//! is never bound. That is the point rather than an implementation detail —
//! purging a network while the API serves history from it would hand clients
//! an empty history that looks authoritative, and the checkpoint-bound cursors
//! would conflict mid-page (`docs/design/019` section 5.3).
//!
//! It is the only way back from a terminal `ConfirmedReorg`: a Bitcoin reorg
//! deeper than the configured `chains.bitcoin.confirmation_depth`, which
//! quarantines that one network for good. The address registry is untouched,
//! so the next ordinary start finds no checkpoint and re-indexes the network
//! from the earliest registered start position.

use std::{ffi::OsString, fmt};

use base::{ChainName, Network};
use indexing::Purging as _;

use crate::config::{AnyError, Config};

/// One `--rescan <chain>/<network>` request, parsed but not yet checked
/// against the configuration.
pub(crate) struct Rescan {
    network: Network,
}

impl Rescan {
    const FLAG: &'static str = "--rescan";

    /// Reads whatever follows the configuration path: nothing at all (an
    /// ordinary server start), or exactly `--rescan <chain>/<network>`.
    ///
    /// Only the spelling is checked here, so a malformed argument is refused
    /// before the configuration file is even read.
    pub(crate) fn parse(
        arguments: impl Iterator<Item = OsString>,
    ) -> Result<Option<Self>, AnyError> {
        let arguments = arguments.collect::<Vec<_>>();
        if arguments.is_empty() {
            return Ok(None);
        }
        let [flag, target] = arguments.as_slice() else {
            return Err(crate::USAGE.into());
        };
        if *flag != *Self::FLAG {
            return Err(crate::USAGE.into());
        }
        let Some(target) = target.to_str() else {
            return Err(Self::malformed("the argument is not valid UTF-8"));
        };
        Self::named(target).map(Some)
    }

    /// Splits `<chain>/<network>` into the identity a repository is opened
    /// with. Both halves are chain-owned canonical text, never a pattern:
    /// this tool deletes, so it names exactly one network or none.
    fn named(target: &str) -> Result<Self, AnyError> {
        let Some((chain, name)) = target.split_once('/') else {
            return Err(Self::malformed(target));
        };
        if chain.trim().is_empty() || name.trim().is_empty() || name.contains('/') {
            return Err(Self::malformed(target));
        }
        Ok(Self {
            network: Network::new(ChainName::new(chain), name),
        })
    }

    fn malformed(target: &str) -> AnyError {
        format!("--rescan takes one <chain>/<network> argument, such as bitcoin/mainnet: {target}")
            .into()
    }

    /// Removes this network's checkpoint, history, movements, and live
    /// outputs, and reports what it removed on standard output.
    ///
    /// Says what it is about to do first, because the delete is not
    /// reversible. Refuses a network the file does not configure rather than
    /// opening a repository for it: a typo must not silently create and then
    /// empty a namespace of its own.
    ///
    /// # Errors
    ///
    /// The refusal above, a storage backend that cannot be opened, or a
    /// removal the backend refused. Nothing is removed unless all of it is.
    pub(crate) async fn purge(&self, config: &Config) -> Result<(), AnyError> {
        let configured = config.chains()?.networks();
        if !configured.contains(&self.network) {
            return Err(format!(
                "{self} is not a configured network; this file configures {}",
                Self::listed(&configured)
            )
            .into());
        }
        println!(
            "payment-server: removing every indexing row of {self}. \
             The address registry and every other network are left alone, \
             and nothing is served while this runs."
        );
        let store = config.storage.open().await?;
        let removed = store.repository().purge_network(&self.network).await?;
        println!(
            "removed {} checkpoint, {} history, {} movement, {} block-credit, \
             and {} output rows",
            removed.checkpoint,
            removed.history,
            removed.movements,
            removed.credits,
            removed.outputs
        );
        println!("the next start re-indexes {self} from the earliest observed start");
        Ok(())
    }

    fn listed(networks: &[Network]) -> String {
        networks
            .iter()
            .map(|network| format!("{}/{}", network.chain, network.name))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl fmt::Display for Rescan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.network.chain, self.network.name)
    }
}

#[cfg(test)]
mod tests {
    use std::{net::TcpListener, path::Path};

    use indexing::{
        Address, AssetId, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef,
        CanonicalBlock, Decimal, IndexStore as _, IndexedBatch, InterpretedBlock, MovementId,
        ObservationDraft, ObservationDraftStatus, OutputChanges, TransactionRef, ValueMovement,
        Watch, WatchStore as _,
    };
    use serde_json::{Value, json};

    use super::*;

    const GENESIS: &str = "11111111111111111111111111111111";

    fn arguments<const N: usize>(values: [&str; N]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn parse<const N: usize>(values: [&str; N]) -> Result<Option<Rescan>, AnyError> {
        Rescan::parse(arguments(values).into_iter())
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
        let interpreted = InterpretedBlock {
            block: BlockRef {
                position: BlockPosition(1),
                height: BlockHeight(1),
                hash: BlockHash(vec![1]),
                parent: Some(BlockParent {
                    position: BlockPosition(0),
                    hash: BlockHash(vec![0]),
                }),
                timestamp: Some(1_000),
            },
            transactions: vec![ObservationDraft {
                transaction_id: TransactionRef::new(network.clone(), "credit"),
                transaction_order: 0,
                status: ObservationDraftStatus::Included,
                movements: vec![ValueMovement::mint(
                    MovementId("0".into()),
                    AssetId::native(network.chain.clone()),
                    Decimal::from(5_u64),
                    Address::new(network.clone(), "owner"),
                )],
                fee: None,
            }],
            outputs: OutputChanges::default(),
            credits: Vec::new(),
        };
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
            Rescan::parse(invalid.iter().map(OsString::from))
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
                error.to_string().starts_with("--rescan takes one"),
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
            .purge(&config)
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
            .purge(&config)
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
}
