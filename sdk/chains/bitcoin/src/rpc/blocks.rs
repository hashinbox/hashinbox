use indexing::{BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, SourceError};

use super::{
    Client,
    error::{map_json_rpc_error, source_error},
    transport::{Client as Transport, RawJson},
    wire::{
        format_bitcoin_block_hash, parse_bitcoin_block_hash, parse_object, required_string,
        required_u64,
    },
};

impl<C> Client<C>
where
    C: Transport,
{
    /// Returns the node's current canonical block hash at `height`.
    ///
    /// A transient height disappearance during a shorter, higher-work reorg is
    /// represented as `None`; callers decide whether to retry or fail closed.
    pub async fn canonical_hash(
        &self,
        height: BlockHeight,
    ) -> Result<Option<BlockHash>, SourceError> {
        let raw = self
            .request_optional_result("getblockhash", serde_json::json!([height.0]), &[-8])
            .await?;
        raw.map(|raw| {
            let encoded: String = raw.deserialize().map_err(map_json_rpc_error)?;
            parse_bitcoin_block_hash(&encoded)
        })
        .transpose()
    }

    pub(crate) async fn header(
        &self,
        expected_hash: &BlockHash,
        expected_height: BlockHeight,
    ) -> Result<BlockRef, SourceError> {
        let raw = self
            .request_result(
                "getblockheader",
                serde_json::json!([format_bitcoin_block_hash(expected_hash)?, true]),
            )
            .await?;
        let header = parse_header(&raw, expected_height)?;
        if header.hash != *expected_hash {
            return Err(source_error(
                "Bitcoin header lookup returned a different block hash",
                true,
            ));
        }
        Ok(header)
    }
}

// Bitcoin's verbose header decoder keeps protocol validation outside the
// chain-neutral BlockRef while preserving field-error precedence.
fn parse_header(raw: &RawJson, expected_height: BlockHeight) -> Result<BlockRef, SourceError> {
    let result = parse_object(raw, "Bitcoin getblockheader result")?;
    let height = BlockHeight(required_u64(
        &result,
        "height",
        "Bitcoin block-header height",
    )?);
    if expected_height != height {
        return Err(source_error(
            "Bitcoin block header does not match the requested height",
            true,
        ));
    }
    let hash = parse_bitcoin_block_hash(&required_string(
        &result,
        "hash",
        "Bitcoin block-header hash",
    )?)?;
    let parent = if height.0 == 0 {
        None
    } else {
        Some(BlockParent {
            position: BlockPosition(height.0 - 1),
            hash: parse_bitcoin_block_hash(&required_string(
                &result,
                "previousblockhash",
                "Bitcoin previous block hash",
            )?)?,
        })
    };
    let timestamp = required_u64(&result, "time", "Bitcoin block-header timestamp")?;
    Ok(BlockRef {
        position: BlockPosition(height.0),
        height,
        hash,
        parent,
        timestamp: Some(timestamp),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures_executor::block_on;
    use json_rpc::{BoxFuture, Call, CallResult, Error, RawJson};
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        Network,
        rpc::{CoreConfig, client::Connection},
    };

    struct Reply(Value);

    impl Reply {
        fn client(self) -> Client<Self> {
            Client {
                connection: Arc::new(Connection {
                    client: self,
                    config: CoreConfig {
                        expected_network: Network::Regtest,
                        expected_genesis_hash: BlockHash(vec![0; 32]),
                    },
                }),
            }
        }
    }

    impl Transport for Reply {
        fn request<'a>(
            &'a self,
            method: &'a str,
            params: Value,
        ) -> BoxFuture<'a, Result<CallResult, Error>> {
            assert_eq!(method, "getblockheader");
            assert_eq!(params, json!(["02".repeat(32), true]));
            Box::pin(async { Ok(Ok(RawJson::from_serializable(&self.0)?)) })
        }

        fn request_once<'a>(
            &'a self,
            _method: &'a str,
            _params: Value,
        ) -> BoxFuture<'a, Result<CallResult, Error>> {
            panic!("header reads keep the normal transport policy")
        }

        fn batch<'a>(&'a self, _calls: Vec<Call>) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
            panic!("a header lookup is one request")
        }
    }

    #[test]
    fn header_field_errors_precede_requested_hash_matching() {
        let hash = "02".repeat(32);
        let different_hash = "03".repeat(32);
        for (value, message) in [
            (
                json!({"height": -1, "hash": false}),
                "Bitcoin block-header height is missing or invalid",
            ),
            (
                json!({"height": 9, "hash": false}),
                "Bitcoin block header does not match the requested height",
            ),
            (
                json!({"height": 10, "hash": false, "previousblockhash": false}),
                "Bitcoin block-header hash is missing or invalid",
            ),
            (
                json!({"height": 10, "hash": "invalid", "previousblockhash": false}),
                "Bitcoin RPC returned an invalid block hash",
            ),
            (
                json!({"height": 10, "hash": different_hash, "previousblockhash": false}),
                "Bitcoin previous block hash is missing or invalid",
            ),
            (
                json!({"height": 10, "hash": different_hash, "previousblockhash": "invalid"}),
                "Bitcoin RPC returned an invalid block hash",
            ),
            (
                json!({
                    "height": 10,
                    "hash": different_hash,
                    "previousblockhash": hash,
                    "time": -1,
                }),
                "Bitcoin block-header timestamp is missing or invalid",
            ),
            (
                json!({"height": 10, "hash": different_hash, "previousblockhash": hash, "time": 0}),
                "Bitcoin header lookup returned a different block hash",
            ),
        ] {
            let client = Reply(value).client();
            let error = block_on(client.header(&BlockHash(vec![2; 32]), BlockHeight(10)))
                .expect_err("invalid header must fail");
            assert_eq!(error.message, message);
            assert!(error.retryable);
        }
    }

    #[test]
    fn genesis_ignores_invalid_parent_but_preserves_timestamp_extremes() {
        for timestamp in [0, u64::MAX] {
            let client = Reply(json!({
                "height": 0,
                "hash": "02".repeat(32),
                "previousblockhash": false,
                "time": timestamp,
            }))
            .client();
            let hash = BlockHash(vec![2; 32]);
            let header = block_on(client.header(&hash, BlockHeight(0))).unwrap();
            assert_eq!(
                header,
                BlockRef {
                    position: BlockPosition(0),
                    height: BlockHeight(0),
                    hash,
                    parent: None,
                    timestamp: Some(timestamp),
                }
            );
        }
    }

    #[test]
    fn malformed_requested_hash_fails_before_transport() {
        let client = Reply(Value::Null).client();
        let error = block_on(client.header(&BlockHash(vec![2; 31]), BlockHeight(0)))
            .expect_err("invalid requested hash must fail before RPC");
        assert_eq!(error.message, "Bitcoin block hash must be 32 bytes");
        assert!(!error.retryable);
    }
}
