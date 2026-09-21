//! Identity and synchronization policy for the Ethereum block indexer.
//!
//! Callers construct and share their RPC transport separately. [`IndexerSettings::source`]
//! verifies chain ID before storage opens; [`IndexerSettings::build`] then combines
//! that verified source with the caller's repository without RPC calls.

use std::sync::Arc;

use indexing::{ChainId, IndexError, IndexErrorKind, IndexScope, Observer, SyncConfig};
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
    /// Depth at which history reports a transaction confirmed.
    pub confirmations: u64,
    /// How far back a reorg can be reversed from the rollback journal.
    pub reorg_retention: u64,
    /// Blocks indexed per `sync` call.
    pub batch_size: usize,
}

impl IndexerSettings {
    /// Uses twelve confirmations, a hundred-block rollback journal, and batches
    /// of a hundred blocks. Transport configuration remains caller-owned.
    #[must_use]
    pub fn new(network: Network) -> Self {
        Self {
            network,
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

    /// Verifies the configured chain ID and returns the exact source that later
    /// enters indexing composition.
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
        })
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
        IndexerSettings::new(Network::new(31_337, "anvil"))
    }

    #[test]
    fn default_source_checks_only_chain_id_and_build_adds_no_rpc() {
        let client = Identity::default();
        let settings = IndexerSettings::new(Network::new(31_337, "anvil"));
        let source = block_on(settings.source(RpcClient::new(client.clone()))).unwrap();
        settings.build(source, (), None).unwrap();
        assert_eq!(client.calls(), ["eth_chainId"]);
    }

    #[test]
    fn source_verifies_chain_id_before_build() {
        let client = Identity::default();
        let settings = IndexerSettings::new(Network::new(1, "anvil"));
        let error = block_on(settings.source(RpcClient::new(client.clone())))
            .err()
            .unwrap();
        assert_eq!(error.kind, IndexErrorKind::Source);
        assert_eq!(
            error.message,
            "Ethereum RPC chain ID does not match configuration"
        );
        assert!(!error.retryable);
        assert_eq!(client.calls(), ["eth_chainId"]);
    }

    #[test]
    fn build_reuses_verified_source_without_more_rpc() {
        let client = Identity::default();
        let settings = settings();
        let source = block_on(settings.source(RpcClient::new(client.clone()))).unwrap();
        assert_eq!(source.config().scope, settings.scope());
        settings.build(source, (), None).unwrap();
        assert_eq!(client.calls(), ["eth_chainId"]);
    }

    #[test]
    fn build_rejects_every_source_identity_mismatch_without_rpc() {
        for mismatch in 0..2 {
            let client = Identity::default();
            let mut settings = settings();
            let source = block_on(settings.source(RpcClient::new(client.clone()))).unwrap();
            if mismatch == 0 {
                settings.network.slug = "another-scope".to_owned();
            } else {
                settings.network.chain_id = 1;
            }
            let error = settings.build(source, (), None).err().unwrap();
            assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
            assert_eq!(
                error.message,
                "Ethereum source belongs to different indexer settings"
            );
            assert!(!error.retryable);
            assert_eq!(client.calls(), ["eth_chainId"]);
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
            assert_eq!(client.calls(), ["eth_chainId"]);
        }
    }
}
