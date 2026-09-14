use indexing::{BlockHeight, SourceError};
use serde_json::{Map, Value};

use crate::Network;

use super::{
    Client, NodeStatus,
    error::{map_json_rpc_error, source_error},
    transport::{Client as Transport, RawJson},
    wire::{parse_bitcoin_block_hash, parse_object, required_bool, required_string, required_u64},
};

const CORE_31_VERSION_MINIMUM: u64 = 310_000;
const CORE_32_VERSION_MINIMUM: u64 = 320_000;

impl<C> Client<C>
where
    C: Transport,
{
    /// Revalidates identity and deployment prerequisites against live node state.
    pub async fn readiness(&self) -> Result<NodeStatus, SourceError> {
        let network_info = self
            .request_result("getnetworkinfo", serde_json::json!([]))
            .await?;
        let network_info = parse_object(&network_info, "Bitcoin getnetworkinfo result")?;
        let version = required_u64(&network_info, "version", "Bitcoin Core version")?;
        if !(CORE_31_VERSION_MINIMUM..CORE_32_VERSION_MINIMUM).contains(&version) {
            return Err(source_error(
                "Bitcoin RPC must be a Bitcoin Core 31.x node",
                false,
            ));
        }

        let chain_info = self
            .request_result("getblockchaininfo", serde_json::json!([]))
            .await?;
        let chain_info = parse_object(&chain_info, "Bitcoin getblockchaininfo result")?;
        let chain = required_string(&chain_info, "chain", "Bitcoin Core chain")?;
        let network = Network::from_core_chain_name(&chain).ok_or_else(|| {
            source_error("Bitcoin Core returned an unsupported chain name", false)
        })?;
        if network != self.connection.config.expected_network {
            return Err(source_error(
                "Bitcoin RPC network does not match configuration",
                false,
            ));
        }
        let status = NodeStatus::from_chain_info(&chain_info, version, network)?;

        let index_info = self
            .request_result("getindexinfo", serde_json::json!(["txindex"]))
            .await?;
        status.require_synced_index(&index_info)?;

        let genesis = self
            .request_result("getblockhash", serde_json::json!([0]))
            .await?;
        let genesis: String = genesis.deserialize().map_err(map_json_rpc_error)?;
        let genesis = parse_bitcoin_block_hash(&genesis)?;
        if genesis != self.connection.config.expected_genesis_hash {
            return Err(source_error(
                "Bitcoin RPC genesis hash does not match configuration",
                false,
            ));
        }

        Ok(status)
    }
}

impl NodeStatus {
    fn from_chain_info(
        chain_info: &Map<String, Value>,
        version: u64,
        network: Network,
    ) -> Result<Self, SourceError> {
        if required_bool(chain_info, "pruned", "Bitcoin pruning status")? {
            return Err(source_error(
                "Bitcoin index source requires an unpruned node",
                false,
            ));
        }
        if required_bool(
            chain_info,
            "initialblockdownload",
            "Bitcoin initial-block-download status",
        )? {
            return Err(source_error(
                "Bitcoin Core is still in initial block download",
                true,
            ));
        }
        let blocks = required_u64(chain_info, "blocks", "Bitcoin block height")?;
        let headers = required_u64(chain_info, "headers", "Bitcoin header height")?;
        if blocks != headers {
            return Err(source_error(
                "Bitcoin Core block and header heights are not synchronized",
                true,
            ));
        }
        let best_block_hash = parse_bitcoin_block_hash(&required_string(
            chain_info,
            "bestblockhash",
            "Bitcoin best block hash",
        )?)?;

        Ok(Self {
            version,
            network,
            height: BlockHeight(blocks),
            best_block_hash,
        })
    }

    fn require_synced_index(&self, index_info: &RawJson) -> Result<(), SourceError> {
        let index_info = parse_object(index_info, "Bitcoin getindexinfo result")?;
        let txindex = index_info
            .get("txindex")
            .and_then(Value::as_object)
            .ok_or_else(|| source_error("Bitcoin Core transaction index is not enabled", false))?;
        if !required_bool(txindex, "synced", "Bitcoin transaction-index status")? {
            return Err(source_error(
                "Bitcoin Core transaction index is not synchronized",
                true,
            ));
        }
        if let Some(index_height) = txindex.get("best_block_height") {
            let index_height = index_height.as_u64().ok_or_else(|| {
                source_error(
                    "Bitcoin transaction-index height is not an unsigned integer",
                    true,
                )
            })?;
            if index_height != self.height.0 {
                return Err(source_error(
                    "Bitcoin Core transaction index has not reached the canonical tip",
                    true,
                ));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use futures_executor::block_on;
    use json_rpc::{BoxFuture, Call, CallResult, Error, RawJson};
    use serde_json::json;

    use super::*;
    use crate::rpc::{CoreConfig, client::Connection};

    struct Replies {
        values: Mutex<VecDeque<Value>>,
        methods: Mutex<VecDeque<(&'static str, Value)>>,
    }

    impl Transport for Replies {
        fn request<'a>(
            &'a self,
            method: &'a str,
            params: Value,
        ) -> BoxFuture<'a, Result<CallResult, Error>> {
            let expected = self.methods.lock().unwrap().pop_front().unwrap();
            assert_eq!((method, params), expected);
            let value = self.values.lock().unwrap().pop_front().expect("extra RPC");
            Box::pin(async move { Ok(Ok(RawJson::from_serializable(&value)?)) })
        }

        fn request_once<'a>(
            &'a self,
            _method: &'a str,
            _params: Value,
        ) -> BoxFuture<'a, Result<CallResult, Error>> {
            panic!("readiness retains ordinary transport execution")
        }

        fn batch<'a>(&'a self, _calls: Vec<Call>) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
            panic!("readiness requests must remain ordered")
        }
    }

    fn chain_info() -> Value {
        json!({
            "chain": "regtest", "pruned": false, "initialblockdownload": false,
            "blocks": 10, "headers": 10, "bestblockhash": "02".repeat(32),
        })
    }

    fn readiness(values: Vec<Value>) -> Result<NodeStatus, SourceError> {
        let methods = VecDeque::from([
            ("getnetworkinfo", json!([])),
            ("getblockchaininfo", json!([])),
            ("getindexinfo", json!(["txindex"])),
            ("getblockhash", json!([0])),
        ]);
        let client = Client {
            connection: std::sync::Arc::new(Connection {
                client: Replies {
                    values: Mutex::new(values.into()),
                    methods: Mutex::new(methods),
                },
                config: CoreConfig {
                    expected_network: Network::Regtest,
                    expected_genesis_hash: indexing::BlockHash(vec![1; 32]),
                },
            }),
        };
        let result = block_on(client.readiness());
        assert!(client.connection.client.values.lock().unwrap().is_empty());
        result
    }

    #[test]
    fn version_errors_stop_before_chain_identity_lookup() {
        for (info, message, retryable) in [
            (
                Value::Null,
                "Bitcoin getnetworkinfo result must be an object",
                true,
            ),
            (
                json!({"version": -1}),
                "Bitcoin Core version is missing or invalid",
                true,
            ),
            (
                json!({"version": 309999}),
                "Bitcoin RPC must be a Bitcoin Core 31.x node",
                false,
            ),
            (
                json!({"version": 320000}),
                "Bitcoin RPC must be a Bitcoin Core 31.x node",
                false,
            ),
        ] {
            let error = readiness(vec![info]).unwrap_err();
            assert_eq!(error.message, message);
            assert_eq!(error.retryable, retryable);
        }
    }

    #[test]
    fn chain_errors_keep_precedence_and_stop_before_index_lookup() {
        for (patch, message, retryable) in [
            (
                json!({"chain": false, "pruned": true}),
                "Bitcoin Core chain is missing or invalid",
                true,
            ),
            (
                json!({"chain": "other", "pruned": true}),
                "Bitcoin Core returned an unsupported chain name",
                false,
            ),
            (
                json!({"chain": "main", "pruned": true}),
                "Bitcoin RPC network does not match configuration",
                false,
            ),
            (
                json!({"pruned": null, "initialblockdownload": true}),
                "Bitcoin pruning status is missing or invalid",
                true,
            ),
            (
                json!({"pruned": true, "initialblockdownload": true}),
                "Bitcoin index source requires an unpruned node",
                false,
            ),
            (
                json!({"initialblockdownload": true, "blocks": false}),
                "Bitcoin Core is still in initial block download",
                true,
            ),
            (
                json!({"blocks": false, "headers": false}),
                "Bitcoin block height is missing or invalid",
                true,
            ),
            (
                json!({"headers": false, "bestblockhash": false}),
                "Bitcoin header height is missing or invalid",
                true,
            ),
            (
                json!({"headers": 11, "bestblockhash": false}),
                "Bitcoin Core block and header heights are not synchronized",
                true,
            ),
            (
                json!({"bestblockhash": "invalid"}),
                "Bitcoin RPC returned an invalid block hash",
                true,
            ),
        ] {
            let mut chain = chain_info();
            chain
                .as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            let error = readiness(vec![json!({"version": 310000}), chain]).unwrap_err();
            assert_eq!(error.message, message);
            assert_eq!(error.retryable, retryable);
        }
    }

    #[test]
    fn index_errors_stop_before_genesis_lookup() {
        for (info, message, retryable) in [
            (
                json!({}),
                "Bitcoin Core transaction index is not enabled",
                false,
            ),
            (
                json!({"txindex": {}}),
                "Bitcoin transaction-index status is missing or invalid",
                true,
            ),
            (
                json!({"txindex": {"synced": false, "best_block_height": -1}}),
                "Bitcoin Core transaction index is not synchronized",
                true,
            ),
            (
                json!({"txindex": {"synced": true, "best_block_height": -1}}),
                "Bitcoin transaction-index height is not an unsigned integer",
                true,
            ),
            (
                json!({"txindex": {"synced": true, "best_block_height": 9}}),
                "Bitcoin Core transaction index has not reached the canonical tip",
                true,
            ),
        ] {
            let error =
                readiness(vec![json!({"version": 310000}), chain_info(), info]).unwrap_err();
            assert_eq!(error.message, message);
            assert_eq!(error.retryable, retryable);
        }
    }

    #[test]
    fn readiness_accepts_core31_boundaries_and_optional_index_height() {
        for version in [310000, 319999] {
            let expected = NodeStatus {
                version,
                network: Network::Regtest,
                height: BlockHeight(10),
                best_block_hash: indexing::BlockHash(vec![2; 32]),
            };
            for index in [
                json!({"txindex": {"synced": true}}),
                json!({"txindex": {"synced": true, "best_block_height": 10}}),
            ] {
                let status = readiness(vec![
                    json!({"version": version}),
                    chain_info(),
                    index,
                    json!("01".repeat(32)),
                ])
                .unwrap();
                assert_eq!(status, expected);
            }
        }
    }

    #[test]
    fn genesis_errors_retain_identity_policy_after_readiness_checks() {
        for (genesis, message, retryable) in [
            (
                "invalid".to_owned(),
                "Bitcoin RPC returned an invalid block hash",
                true,
            ),
            (
                "02".repeat(32),
                "Bitcoin RPC genesis hash does not match configuration",
                false,
            ),
        ] {
            let error = readiness(vec![
                json!({"version": 310000}),
                chain_info(),
                json!({"txindex": {"synced": true}}),
                json!(genesis),
            ])
            .unwrap_err();
            assert_eq!(error.message, message);
            assert_eq!(error.retryable, retryable);
        }
    }
}
