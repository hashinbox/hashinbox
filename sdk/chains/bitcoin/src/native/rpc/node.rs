//! Node identity and readiness: what [`Client::connect`] verifies.

use base::{BlockHash, BlockHeight};
use serde_json::{Value, json};

use super::{Client, wire::parse_block_hash};
use crate::native::{Error, ErrorKind, Network, json::Object};

/// Supported Bitcoin Core versions: 31.x.
const CORE_VERSIONS: std::ops::Range<u64> = 310_000..320_000;

/// A ready node's identity and tip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// Bitcoin Core's numeric version, e.g. `310000`.
    pub version: u64,
    /// The network the node serves.
    pub network: Network,
    /// Its validated tip height.
    pub height: BlockHeight,
    /// Its validated tip.
    pub best_block_hash: BlockHash,
}

impl<C: json_rpc::Client> Client<C> {
    /// Revalidates identity and deployment prerequisites against live node state.
    ///
    /// # Errors
    ///
    /// As for [`Client::connect`].
    pub async fn status(&self) -> Result<Status, Error> {
        let info = self.request("getnetworkinfo", json!([])).await?;
        let info = Object::new(&info, "Bitcoin getnetworkinfo result", Error::invalid_reply)?;
        let version = info.u64("version", "Bitcoin Core version")?;
        if !CORE_VERSIONS.contains(&version) {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Bitcoin RPC must be a Bitcoin Core 31.x node",
            ));
        }
        let chain = self.request("getblockchaininfo", json!([])).await?;
        let chain = Object::new(
            &chain,
            "Bitcoin getblockchaininfo result",
            Error::invalid_reply,
        )?;
        let status = self.chain_status(&chain, version)?;
        let index = self.request("getindexinfo", json!(["txindex"])).await?;
        status.require_synced_index(&index)?;
        Ok(status)
    }

    fn chain_status(&self, chain: &Object<'_>, version: u64) -> Result<Status, Error> {
        let name = chain.string("chain", "Bitcoin Core chain")?;
        let network = Network::from_core_chain(name).ok_or_else(|| {
            Error::new(
                ErrorKind::Divergent,
                "Bitcoin Core returned an unsupported chain name",
            )
        })?;
        if network != *self.network() {
            return Err(Error::new(
                ErrorKind::Divergent,
                "Bitcoin RPC network does not match configuration",
            ));
        }
        if chain.bool("pruned", "Bitcoin pruning status")? {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Bitcoin index source requires an unpruned node",
            ));
        }
        let downloading = "Bitcoin initial-block-download status";
        if chain.bool("initialblockdownload", downloading)? {
            return Err(Error::rpc(
                "Bitcoin Core is still in initial block download",
                true,
            ));
        }
        let blocks = chain.u64("blocks", "Bitcoin block height")?;
        if blocks != chain.u64("headers", "Bitcoin header height")? {
            return Err(Error::rpc(
                "Bitcoin Core block and header heights are not synchronized",
                true,
            ));
        }
        let hash = chain.string("bestblockhash", "Bitcoin best block hash")?;
        Ok(Status {
            version,
            network,
            height: BlockHeight(blocks),
            best_block_hash: parse_block_hash(hash)
                .ok_or_else(|| chain.fail("Bitcoin block hash is invalid"))?,
        })
    }
}

impl Status {
    fn require_synced_index(&self, info: &Value) -> Result<(), Error> {
        let info = Object::new(info, "Bitcoin getindexinfo result", Error::invalid_reply)?;
        let index = info
            .get("txindex")
            .filter(|index| index.is_object())
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidConfiguration,
                    "Bitcoin Core transaction index is not enabled",
                )
            })?;
        let index = Object::new(index, "Bitcoin transaction index", Error::invalid_reply)?;
        if !index.bool("synced", "Bitcoin transaction-index status")? {
            return Err(Error::rpc(
                "Bitcoin Core transaction index is not synchronized",
                true,
            ));
        }
        let Some(height) = index.get("best_block_height") else {
            return Ok(());
        };
        let height = height.as_u64().ok_or_else(|| {
            index.fail("Bitcoin transaction-index height is not an unsigned integer")
        })?;
        if height != self.height.0 {
            return Err(Error::rpc(
                "Bitcoin Core transaction index has not reached the canonical tip",
                true,
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use futures_executor::block_on;

    use super::*;
    use crate::native::rpc::script::{Reply, Script, Scripted};

    fn chain_info() -> Value {
        json!({
            "chain": "regtest", "pruned": false, "initialblockdownload": false,
            "blocks": 10, "headers": 10, "bestblockhash": "02".repeat(32),
        })
    }

    fn status(replies: Vec<Value>) -> Result<Status, Error> {
        let methods = [
            ("getnetworkinfo", json!([])),
            ("getblockchaininfo", json!([])),
            ("getindexinfo", json!(["txindex"])),
        ];
        let script = Script::node(
            methods
                .into_iter()
                .zip(replies)
                .map(|((method, params), value)| Reply::ok(method, value).expecting(params)),
        );
        let result = block_on(script.client().status());
        script.exhausted();
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
            let error = status(vec![info]).unwrap_err();
            assert_eq!(
                (error.message.as_str(), error.retryable),
                (message, retryable)
            );
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
                "Bitcoin block hash is invalid",
                true,
            ),
        ] {
            let mut chain = chain_info();
            chain
                .as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            let error = status(vec![json!({"version": 310000}), chain]).unwrap_err();
            assert_eq!(
                (error.message.as_str(), error.retryable),
                (message, retryable)
            );
        }
    }

    #[test]
    fn index_errors_keep_their_order() {
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
            let error = status(vec![json!({"version": 310000}), chain_info(), info]).unwrap_err();
            assert_eq!(
                (error.message.as_str(), error.retryable),
                (message, retryable)
            );
        }
    }

    #[test]
    fn status_accepts_core31_boundaries_and_optional_index_height() {
        for version in [310000, 319999] {
            for index in [
                json!({"txindex": {"synced": true}}),
                json!({"txindex": {"synced": true, "best_block_height": 10}}),
            ] {
                let status =
                    status(vec![json!({"version": version}), chain_info(), index]).unwrap();
                assert_eq!(
                    status,
                    Status {
                        version,
                        network: Network::Regtest,
                        height: BlockHeight(10),
                        best_block_hash: BlockHash(vec![2; 32]),
                    }
                );
            }
        }
    }
}
