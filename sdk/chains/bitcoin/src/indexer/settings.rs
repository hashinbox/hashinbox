//! Bitcoin indexing policy over an already verified RPC client.
//!
//! The embedding application owns transport construction and connects its shared
//! RPC client before opening storage. These settings own only indexing policy.

use std::sync::Arc;

use indexing::{ChainId, IndexError, IndexErrorKind, IndexScope, Observer, SyncConfig};

use crate::{BlockInterpreter, Indexer, Network, RpcClient, Source, SourceConfig};

/// Synchronization and confirmation policy for one Bitcoin network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexerSettings {
    /// Must match the already verified RPC client.
    pub network: Network,
    /// Depth at which history reports a transaction confirmed.
    pub confirmations: u64,
    /// How far back a reorg can be reversed from the rollback journal.
    pub reorg_retention: u64,
    /// Blocks indexed per `sync` call.
    pub batch_size: usize,
}

impl IndexerSettings {
    /// Uses six confirmations, a hundred-block rollback journal, and batches of a hundred blocks.
    #[must_use]
    pub const fn new(network: Network) -> Self {
        Self {
            network,
            confirmations: 6,
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
            network: self.network.canonical_name().to_owned(),
        }
    }

    /// Builds an indexer on an already-connected client.
    ///
    /// Takes the optional genesis pin from the client without making another
    /// RPC request, so the source cannot disagree with its connection.
    ///
    /// `observer`, when supplied, is called after each block this indexer
    /// commits. Pass `None` to index without notification.
    pub fn build<C, R>(
        &self,
        client: RpcClient<C>,
        repository: R,
        observer: Option<Arc<dyn Observer>>,
    ) -> Result<Indexer<C, R>, IndexError>
    where
        C: json_rpc::Client,
        R: Clone,
    {
        if client.config().expected_network != self.network {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "Bitcoin client is connected to a different network than these settings",
                false,
            ));
        }
        let scope = self.scope();
        let source = Source::from_client(
            client.clone(),
            SourceConfig {
                scope: scope.clone(),
                network: self.network,
                expected_genesis_hash: client.config().expected_genesis_hash.clone(),
            },
        )?;
        let mut indexer = Indexer::new(
            source,
            BlockInterpreter::new(scope.clone(), self.network)?,
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
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures_executor::block_on;
    use json_rpc::{BoxFuture, Call, CallResult, Error, RawJson};
    use serde_json::{Value, json};

    use super::*;
    use crate::CoreConfig;

    struct Readiness {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl json_rpc::Client for Readiness {
        fn request<'a>(
            &'a self,
            method: &'a str,
            params: Value,
        ) -> BoxFuture<'a, Result<CallResult, Error>> {
            let mut calls = self.calls.lock().unwrap();
            let result = match calls.len() {
                0 => {
                    assert_eq!((method, params), ("getnetworkinfo", json!([])));
                    json!({"version": 310000})
                }
                1 => {
                    assert_eq!((method, params), ("getblockchaininfo", json!([])));
                    json!({
                        "chain": "regtest", "blocks": 10, "headers": 10,
                        "bestblockhash": "02".repeat(32),
                        "initialblockdownload": false, "pruned": false,
                    })
                }
                2 => {
                    assert_eq!((method, params), ("getindexinfo", json!(["txindex"])));
                    json!({"txindex": {"synced": true, "best_block_height": 10}})
                }
                3 => {
                    assert_eq!((method, params), ("getblockhash", json!([0])));
                    json!("01".repeat(32))
                }
                _ => panic!("building the indexer must not repeat verified RPC checks"),
            };
            calls.push(method.to_owned());
            Box::pin(async move { Ok(Ok(RawJson::from_serializable(&result)?)) })
        }

        fn request_once<'a>(
            &'a self,
            _: &'a str,
            _: Value,
        ) -> BoxFuture<'a, Result<CallResult, Error>> {
            panic!("indexer construction must not submit transactions")
        }

        fn batch<'a>(&'a self, _: Vec<Call>) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
            panic!("readiness checks must retain their ordered requests")
        }
    }

    #[test]
    fn construction_uses_verified_client_and_validates_network_before_policy() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let client = block_on(RpcClient::connect_with(
            Readiness {
                calls: calls.clone(),
            },
            CoreConfig::new(Network::Regtest)
                .with_genesis_hash(&"01".repeat(32))
                .unwrap(),
        ))
        .unwrap();
        let mut settings = IndexerSettings::new(Network::Regtest);
        assert_eq!(settings.scope().network, "regtest");
        assert_eq!(settings.scope().chain.0, crate::CHAIN);
        assert_eq!(
            (
                settings.confirmations,
                settings.reorg_retention,
                settings.batch_size
            ),
            (6, 100, 100)
        );
        settings.build(client.clone(), (), None).unwrap();

        settings.network = Network::Mainnet;
        settings.batch_size = 0;
        let error = settings.build(client.clone(), (), None).err().unwrap();
        assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
        assert_eq!(
            error.message,
            "Bitcoin client is connected to a different network than these settings"
        );
        assert!(!error.retryable);

        settings.network = Network::Regtest;
        let error = settings.build(client, (), None).err().unwrap();
        let expected = SyncConfig::new(
            settings.scope(),
            settings.confirmations,
            settings.reorg_retention,
            0,
        )
        .unwrap_err();
        assert_eq!(error, expected);
        assert_eq!(calls.lock().unwrap().len(), 4);
    }

    #[test]
    fn invalid_core_identity_fails_before_rpc() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let result = block_on(RpcClient::connect_with(
            Readiness {
                calls: calls.clone(),
            },
            CoreConfig {
                expected_network: Network::Regtest,
                expected_genesis_hash: Some(indexing::BlockHash(vec![0; 31])),
            },
        ));
        let error = result.err().unwrap();
        assert_eq!(
            error.message,
            "configured Bitcoin genesis hash must be 32 bytes"
        );
        assert!(!error.retryable);
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn source_and_indexer_preserve_the_clients_optional_genesis_pin() {
        for pin in [None, Some("01".repeat(32))] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let mut config = CoreConfig::new(Network::Regtest);
            if let Some(pin) = &pin {
                config = config.with_genesis_hash(pin).unwrap();
            }
            let client = block_on(RpcClient::connect_with(
                Readiness {
                    calls: calls.clone(),
                },
                config.clone(),
            ))
            .unwrap();
            let settings = IndexerSettings::new(Network::Regtest);
            settings.build(client.clone(), (), None).unwrap();
            let source_config = SourceConfig {
                scope: settings.scope(),
                network: settings.network,
                expected_genesis_hash: config.expected_genesis_hash.clone(),
            };
            let source = Source::from_client(client.clone(), source_config.clone()).unwrap();
            assert_eq!(source.config(), &source_config);
            for mismatched in [None, Some(indexing::BlockHash(vec![2; 32]))] {
                if mismatched == config.expected_genesis_hash {
                    continue;
                }
                let mut wrong = source_config.clone();
                wrong.expected_genesis_hash = mismatched;
                let error = Source::from_client(client.clone(), wrong).err().unwrap();
                assert_eq!(
                    error.message,
                    "Bitcoin block adapter configuration does not match its RPC client"
                );
                assert!(!error.retryable);
            }
            assert_eq!(
                calls.lock().unwrap().len(),
                if pin.is_some() { 4 } else { 3 }
            );
        }
    }
}
