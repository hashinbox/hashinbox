//! Identity and synchronization policy for the Ethereum block indexer.
//!
//! Callers construct and share their RPC transport separately. [`IndexerSettings::source`]
//! verifies chain ID and an optional genesis pin before storage opens; [`IndexerSettings::build`]
//! then combines that verified source with the caller's repository without RPC calls.

use std::sync::Arc;

use indexing::{BlockHash, ChainId, IndexError, IndexErrorKind, IndexScope, Observer, SyncConfig};
use json_rpc::Client;

use crate::{BlockInterpreter, Indexer, Network, RpcClient, Source, SourceConfig};

/// Expected Ethereum identity and indexing policy, independent of RPC transport.
///
/// Every field is public, so a caller may take [`IndexerSettings::new`] and
/// override only what differs from the defaults.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexerSettings {
    /// Verified against `eth_chainId` when the source is built; its slug
    /// becomes the scope's network and part of the storage keyspace.
    pub network: Network,
    /// Optional block-zero hash pin, verified when the source is built.
    /// Without a pin, construction verifies only the chain ID.
    pub genesis_hash: Option<String>,
    /// Depth at which history reports a transaction confirmed.
    pub confirmations: u64,
    /// How far back a reorg can be reversed from the rollback journal.
    pub reorg_retention: u64,
    /// Blocks indexed per `sync` call.
    pub batch_size: usize,
}

impl IndexerSettings {
    /// Uses twelve confirmations, a hundred-block rollback journal, and batches
    /// of a hundred blocks. Genesis pinning is opt-in; transport configuration
    /// remains caller-owned.
    #[must_use]
    pub fn new(network: Network) -> Self {
        Self {
            network,
            genesis_hash: None,
            confirmations: 12,
            reorg_retention: 100,
            batch_size: 100,
        }
    }

    /// The scope this indexer owns. Open the repository with it, so the store
    /// and the indexer cannot disagree about which chain they hold.
    #[must_use]
    pub fn scope(&self) -> IndexScope {
        IndexScope {
            chain: ChainId(crate::CHAIN.to_owned()),
            network: self.network.slug.clone(),
        }
    }

    /// Verifies the configured chain ID and optional genesis pin and returns the exact
    /// source that later enters indexing composition.
    pub async fn source<C: Client>(&self, client: RpcClient<C>) -> Result<Source<C>, IndexError> {
        Ok(Source::from_rpc(client, self.source_config()?).await?)
    }

    /// Builds an indexer from a source that was verified before storage opened.
    ///
    /// `observer`, when supplied, is called after each block this indexer
    /// commits. Pass `None` to index without notification.
    pub fn build<C: Client, R>(
        &self,
        source: Source<C>,
        repository: R,
        observer: Option<Arc<dyn Observer>>,
    ) -> Result<Indexer<C, R>, IndexError>
    where
        R: Clone,
    {
        let scope = self.scope();
        if source.config() != &self.source_config()? {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "Ethereum source belongs to different indexer settings",
                false,
            ));
        }
        let mut indexer = Indexer::new(
            source,
            BlockInterpreter::new(scope.clone())?,
            repository,
            SyncConfig::new(
                scope,
                self.confirmations,
                self.reorg_retention,
                self.batch_size,
            )?,
        );
        if let Some(observer) = observer {
            indexer.observe(observer);
        }
        Ok(indexer)
    }

    fn source_config(&self) -> Result<SourceConfig, IndexError> {
        Ok(SourceConfig {
            scope: self.scope(),
            expected_chain_id: self.network.chain_id,
            expected_genesis_hash: self.genesis()?,
        })
    }

    fn genesis(&self) -> Result<Option<BlockHash>, IndexError> {
        let Some(genesis_hash) = self.genesis_hash.as_deref() else {
            return Ok(None);
        };
        let digits = genesis_hash.strip_prefix("0x").unwrap_or(genesis_hash);
        let bytes = alloy_primitives::hex::decode(digits).map_err(|_| {
            IndexError::new(
                IndexErrorKind::InvalidRequest,
                "Ethereum genesis hash must be hexadecimal",
                false,
            )
        })?;
        if bytes.len() != 32 {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "Ethereum genesis hash must contain exactly 32 bytes",
                false,
            ));
        }
        Ok(Some(BlockHash(bytes)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures_executor::block_on;
    use json_rpc::{BoxFuture, Call, CallResult, Error, RawJson};
    use serde_json::{Value, json};

    use super::*;

    #[derive(Clone, Default)]
    struct Identity {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl Identity {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Client for Identity {
        fn request<'a>(
            &'a self,
            method: &'a str,
            params: Value,
        ) -> BoxFuture<'a, Result<CallResult, Error>> {
            self.calls.lock().unwrap().push(method.to_owned());
            let result = match method {
                "eth_chainId" => {
                    assert_eq!(params, json!([]));
                    json!("0x7a69")
                }
                "eth_getBlockByNumber" => {
                    assert_eq!(params, json!(["0x0", false]));
                    json!({
                        "hash": format!("0x{}", "11".repeat(32)),
                        "parentHash": format!("0x{}", "00".repeat(32)),
                        "sha3Uncles": format!("0x{}", "00".repeat(32)),
                        "miner": format!("0x{}", "00".repeat(20)),
                        "stateRoot": format!("0x{}", "00".repeat(32)),
                        "transactionsRoot": format!("0x{}", "00".repeat(32)),
                        "receiptsRoot": format!("0x{}", "00".repeat(32)),
                        "logsBloom": format!("0x{}", "00".repeat(256)),
                        "difficulty": "0x0",
                        "number": "0x0",
                        "gasLimit": "0x1c9c380",
                        "gasUsed": "0x0",
                        "timestamp": "0x0",
                        "extraData": "0x",
                        "mixHash": format!("0x{}", "00".repeat(32)),
                        "nonce": "0x0000000000000000",
                        "uncles": [],
                        "transactions": []
                    })
                }
                _ => panic!("source construction must only verify identity"),
            };
            Box::pin(async move { Ok(Ok(RawJson::from_serializable(&result).unwrap())) })
        }

        fn request_once<'a>(
            &'a self,
            _method: &'a str,
            _params: Value,
        ) -> BoxFuture<'a, Result<CallResult, Error>> {
            unreachable!("source construction must not submit a transaction")
        }

        fn batch<'a>(&'a self, _calls: Vec<Call>) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
            unreachable!("source construction must not fetch receipts")
        }
    }

    fn settings() -> IndexerSettings {
        let mut settings = IndexerSettings::new(Network::new(31_337, "anvil"));
        settings.genesis_hash = Some("11".repeat(32));
        settings
    }

    #[test]
    fn default_source_checks_only_chain_id_and_build_adds_no_rpc() {
        let client = Identity::default();
        let settings = IndexerSettings::new(Network::new(31_337, "anvil"));
        assert_eq!(settings.genesis_hash, None);
        let source = block_on(settings.source(RpcClient::new(client.clone()))).unwrap();
        assert_eq!(source.config().expected_genesis_hash, None);
        settings.build(source, (), None).unwrap();
        assert_eq!(client.calls(), ["eth_chainId"]);
    }

    #[test]
    fn source_verifies_chain_id_and_genesis_before_build() {
        for (chain_id, genesis, methods, message) in [
            (
                1,
                Some("11".repeat(32)),
                vec!["eth_chainId"],
                "Ethereum RPC chain ID does not match configuration",
            ),
            (
                31_337,
                Some("22".repeat(32)),
                vec!["eth_chainId", "eth_getBlockByNumber"],
                "Ethereum RPC genesis hash does not match configuration",
            ),
            (
                1,
                None,
                vec!["eth_chainId"],
                "Ethereum RPC chain ID does not match configuration",
            ),
        ] {
            let client = Identity::default();
            let mut settings = IndexerSettings::new(Network::new(chain_id, "anvil"));
            settings.genesis_hash = genesis;
            let error = block_on(settings.source(RpcClient::new(client.clone())))
                .err()
                .unwrap();
            assert_eq!(error.kind, IndexErrorKind::Source);
            assert_eq!(error.message, message);
            assert!(!error.retryable);
            assert_eq!(client.calls(), methods);
        }
    }

    #[test]
    fn malformed_genesis_fails_before_rpc() {
        for (genesis, message) in [
            ("invalid", "Ethereum genesis hash must be hexadecimal"),
            (
                "0x11",
                "Ethereum genesis hash must contain exactly 32 bytes",
            ),
        ] {
            let client = Identity::default();
            let mut settings = IndexerSettings::new(Network::new(31_337, "anvil"));
            settings.genesis_hash = Some(genesis.to_owned());
            let error = block_on(settings.source(RpcClient::new(client.clone())))
                .err()
                .unwrap();
            assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
            assert_eq!(error.message, message);
            assert!(!error.retryable);
            assert!(client.calls().is_empty());
        }
    }

    #[test]
    fn build_reuses_verified_source_without_more_rpc() {
        let client = Identity::default();
        let settings = settings();
        let source = block_on(settings.source(RpcClient::new(client.clone()))).unwrap();
        assert_eq!(source.config().scope, settings.scope());
        settings.build(source, (), None).unwrap();
        assert_eq!(client.calls(), ["eth_chainId", "eth_getBlockByNumber"]);
    }

    #[test]
    fn build_rejects_every_source_identity_mismatch_without_rpc() {
        for mismatch in 0..3 {
            let client = Identity::default();
            let mut settings = settings();
            let source = block_on(settings.source(RpcClient::new(client.clone()))).unwrap();
            match mismatch {
                0 => settings.network.slug = "another-scope".to_owned(),
                1 => settings.network.chain_id = 1,
                _ => settings.genesis_hash = Some("22".repeat(32)),
            }
            let error = settings.build(source, (), None).err().unwrap();
            assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
            assert_eq!(
                error.message,
                "Ethereum source belongs to different indexer settings"
            );
            assert!(!error.retryable);
            assert_eq!(client.calls(), ["eth_chainId", "eth_getBlockByNumber"]);
        }
    }

    #[test]
    fn build_rejects_changed_genesis_pinning_policy_without_rpc() {
        for pinned in [false, true] {
            let client = Identity::default();
            let mut settings = IndexerSettings::new(Network::new(31_337, "anvil"));
            settings.genesis_hash = pinned.then(|| "11".repeat(32));
            let source = block_on(settings.source(RpcClient::new(client.clone()))).unwrap();
            let verified_calls = client.calls();
            settings.genesis_hash = (!pinned).then(|| "11".repeat(32));

            let error = settings.build(source, (), None).err().unwrap();
            assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
            assert_eq!(
                error.message,
                "Ethereum source belongs to different indexer settings"
            );
            assert!(!error.retryable);
            assert_eq!(client.calls(), verified_calls);
        }
    }

    #[test]
    fn build_rejects_invalid_sync_policy_without_rpc() {
        for (confirmations, retention, batch, message) in [
            (
                0,
                100,
                100,
                "minimum confirmations must be greater than zero",
            ),
            (
                12,
                0,
                100,
                "rollback retention and batch size must be greater than zero",
            ),
            (
                12,
                100,
                0,
                "rollback retention and batch size must be greater than zero",
            ),
        ] {
            let client = Identity::default();
            let mut settings = settings();
            let source = block_on(settings.source(RpcClient::new(client.clone()))).unwrap();
            settings.confirmations = confirmations;
            settings.reorg_retention = retention;
            settings.batch_size = batch;
            let error = settings.build(source, (), None).err().unwrap();
            assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
            assert_eq!(error.message, message);
            assert!(!error.retryable);
            assert_eq!(client.calls(), ["eth_chainId", "eth_getBlockByNumber"]);
        }
    }
}
