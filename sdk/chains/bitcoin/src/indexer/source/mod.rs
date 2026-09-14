use std::collections::{BTreeMap, BTreeSet};

use futures_util::{StreamExt, stream};
use indexing::{
    BlockHash, BlockHeight, BlockPosition, BlockRef, BlockSource, BoxFuture, ChainId, IndexScope,
    SourceError,
};
use json_rpc::Client;
use serde_json::Value;

use crate::{Network, TransactionId};

use super::Block;
use crate::rpc::{
    Client as RpcClient, CoreConfig, format_bitcoin_block_hash, parse_bitcoin_block_hash,
    source_error,
};

// Every non-coinbase input consumes at least a 36-byte outpoint, one-byte
// empty-script length, and four-byte sequence in non-witness serialization.
// At four weight units per byte, a 4,000,000-WU block can therefore contain at
// most floor(4,000,000 / 164) = 24,390 external prevouts even before mandatory
// block and transaction overhead. This deliberately loose ceiling remains
// complete for every consensus-valid block.
const MAX_EXTERNAL_PREVOUTS_PER_BLOCK: usize = 25_000;
const MAX_IN_FLIGHT_PREVOUT_REQUESTS: usize = 4;
const MAX_COMPACT_ADDRESS_BYTES: usize = 128;
const MAX_COMPACT_PREVOUT_JSON_BYTES: usize = 192;
const MAX_COMPACT_PREVOUT_TOTAL_BYTES: usize =
    MAX_EXTERNAL_PREVOUTS_PER_BLOCK * MAX_COMPACT_PREVOUT_JSON_BYTES;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub scope: IndexScope,
    pub network: Network,
    pub expected_genesis_hash: BlockHash,
}

impl Config {
    pub fn validate(&self) -> Result<(), SourceError> {
        if self.scope.chain != ChainId(crate::CHAIN.to_owned()) {
            return Err(source_error(
                "Bitcoin index source scope must use the bitcoin chain ID",
                false,
            ));
        }
        if self.scope.network != self.network.canonical_name() {
            return Err(source_error(
                "Bitcoin index source scope network does not match configuration",
                false,
            ));
        }
        CoreConfig {
            expected_network: self.network,
            expected_genesis_hash: self.expected_genesis_hash.clone(),
        }
        .validate()
    }
}

/// Canonical numbered-block source using bounded Bitcoin Core 31 RPC results.
///
/// The source fetches the block at verbosity 2, then resolves only external
/// previous transactions through at most four concurrent, transport-bounded calls.
/// Full historical scripts are validated and discarded at the source boundary;
/// parsed blocks contain only the value/address facts interpretation needs.
pub struct Blocks<C> {
    client: RpcClient<C>,
    config: Config,
}

impl<C> Blocks<C>
where
    C: Client,
{
    pub async fn connect(client: C, config: Config) -> Result<Self, SourceError> {
        config.validate()?;
        let client = RpcClient::connect(
            client,
            CoreConfig {
                expected_network: config.network,
                expected_genesis_hash: config.expected_genesis_hash.clone(),
            },
        )
        .await?;
        Ok(Self { client, config })
    }

    /// Builds block reads over an already connected shared RPC client.
    pub fn from_client(client: RpcClient<C>, config: Config) -> Result<Self, SourceError> {
        config.validate()?;
        let expected = client.config();
        if expected.expected_network != config.network
            || expected.expected_genesis_hash != config.expected_genesis_hash
        {
            return Err(source_error(
                "Bitcoin block adapter configuration does not match its RPC client",
                false,
            ));
        }
        Ok(Self { client, config })
    }

    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    #[must_use]
    pub fn client(&self) -> &RpcClient<C> {
        &self.client
    }

    async fn block_count(&self) -> Result<BlockHeight, SourceError> {
        self.client
            .request_result("getblockcount", serde_json::json!([]))
            .await?
            .deserialize::<u64>()
            .map(BlockHeight)
            .map_err(|error| source_error(error.to_string(), true))
    }

    async fn optional_hash_at(
        &self,
        height: BlockHeight,
    ) -> Result<Option<BlockHash>, SourceError> {
        let raw = self
            .client
            .request_optional_result("getblockhash", serde_json::json!([height.0]), &[-8])
            .await?;
        raw.map(|raw| {
            let hash: String = raw
                .deserialize()
                .map_err(|error| source_error(error.to_string(), true))?;
            parse_bitcoin_block_hash(&hash)
        })
        .transpose()
    }

    async fn hash_at(&self, height: BlockHeight) -> Result<BlockHash, SourceError> {
        self.optional_hash_at(height).await?.ok_or_else(|| {
            source_error(
                "Bitcoin canonical height changed while its hash was being read",
                true,
            )
        })
    }

    async fn raw_block(&self, hash: &BlockHash) -> Result<Option<Vec<u8>>, SourceError> {
        self.client
            .request_optional_result(
                "getblock",
                serde_json::json!([format_bitcoin_block_hash(hash)?, 2]),
                &[-5],
            )
            .await
            .map(|raw| raw.map(|raw| raw.into_bytes()))
    }

    async fn previous_transaction(
        &self,
        transaction_id: TransactionId,
    ) -> Result<bitcoin::Transaction, SourceError> {
        let raw = self
            .client
            .request_optional_result(
                "getrawtransaction",
                serde_json::json!([transaction_id.to_string(), true]),
                &[-5],
            )
            .await?
            .ok_or_else(|| {
                source_error(
                    format!(
                        "Bitcoin previous transaction {transaction_id} disappeared \
                         during block ingestion"
                    ),
                    true,
                )
            })?;
        let value: Value = raw
            .deserialize()
            .map_err(|error| source_error(error.to_string(), true))?;
        let object = value.as_object().ok_or_else(|| {
            source_error("Bitcoin getrawtransaction result must be an object", true)
        })?;
        let returned_id = required_string(object, "txid", "Bitcoin previous transaction ID")?
            .parse::<TransactionId>()
            .map_err(|_| source_error("Bitcoin previous transaction ID is invalid", true))?;
        if returned_id != transaction_id {
            return Err(source_error(
                "Bitcoin previous-transaction lookup returned a different transaction ID",
                true,
            ));
        }
        let transaction = decode_consensus_transaction(object, transaction_id)?;
        let confirmed_block_hash = required_string(
            object,
            "blockhash",
            "Bitcoin previous transaction block hash",
        )?;
        parse_bitcoin_block_hash(confirmed_block_hash)?;

        Ok(transaction)
    }

    async fn resolve_outputs(
        &self,
        transaction_id: TransactionId,
        required_outputs: &BTreeSet<u32>,
    ) -> Result<BTreeMap<u32, ResolvedOutput>, SourceError> {
        let transaction = self.previous_transaction(transaction_id).await?;
        required_outputs
            .iter()
            .map(|output_index| {
                ResolvedOutput::from_transaction(
                    &transaction,
                    transaction_id,
                    *output_index,
                    self.config.network,
                )
                .map(|output| (*output_index, output))
            })
            .collect()
    }

    async fn enrich_prevouts(&self, raw_block: Vec<u8>) -> Result<Vec<u8>, SourceError> {
        let mut value: Value = serde_json::from_slice(&raw_block)
            .map_err(|_| source_error("Bitcoin block result is not valid JSON", true))?;
        let transactions = Transactions::from_block(&value)?;
        let external_outputs = transactions.required_prevouts()?;

        let mut resolved = ResolvedPrevouts::default();
        let requests =
            external_outputs
                .into_iter()
                .map(|(transaction_id, output_indexes)| async move {
                    let data = self
                        .resolve_outputs(transaction_id, &output_indexes)
                        .await?;
                    Ok::<_, SourceError>((transaction_id, data))
                });
        let mut requests = stream::iter(requests).buffer_unordered(MAX_IN_FLIGHT_PREVOUT_REQUESTS);
        while let Some(result) = requests.next().await {
            let (transaction_id, data) = result?;
            resolved.insert(transaction_id, data);
        }

        resolved.enrich_block(&mut value, &transactions)?;

        serde_json::to_vec(&value)
            .map_err(|_| source_error("Bitcoin enriched block JSON could not be encoded", true))
    }

    async fn fetch_block(&self, height: BlockHeight) -> Result<Block, SourceError> {
        let first_hash = self.hash_at(height).await?;
        let Some(raw_block) = self.raw_block(&first_hash).await? else {
            return Err(source_error(
                "Bitcoin Core no longer exposes the requested block",
                true,
            ));
        };
        let raw_block = self.enrich_prevouts(raw_block).await?;
        let block = Block::parse(
            &raw_block,
            Some(height),
            Some(&first_hash),
            self.config.network,
        )
        .map_err(|error| source_error(error.to_string(), true))?;
        drop(raw_block);
        let second_hash = self.hash_at(height).await?;
        if first_hash != second_hash {
            return Err(source_error(
                "Bitcoin canonical block changed while it was being fetched",
                true,
            ));
        }
        Ok(block)
    }
}

mod prevout;

use prevout::*;
impl<C> BlockSource for Blocks<C>
where
    C: Client,
{
    type Block = Block;

    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        Box::pin(async move {
            let height = self.block_count().await?;
            let hash = self.hash_at(height).await?;
            self.client.header(&hash, height).await
        })
    }

    fn blocks<'a>(
        &'a self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Self::Block>, SourceError>> {
        Box::pin(async move {
            if limit == 0 || start > end {
                return Err(source_error(
                    "Bitcoin block range requires ordered positions and a positive limit",
                    false,
                ));
            }
            let mut position = start.0;
            let end = end.0;
            let mut blocks = Vec::with_capacity(limit.min(64));
            while position <= end && blocks.len() < limit {
                let block = self.fetch_block(BlockHeight(position)).await?;
                blocks.push(block);
                let Some(next) = position.checked_add(1) else {
                    break;
                };
                position = next;
            }
            Ok(blocks)
        })
    }

    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, SourceError>> {
        Box::pin(async move {
            let height = BlockHeight(position.0);
            if height > self.block_count().await? {
                return Ok(None);
            }
            let Some(hash) = self.optional_hash_at(height).await? else {
                return Ok(None);
            };
            self.client.header(&hash, height).await.map(Some)
        })
    }
}

#[cfg(test)]
#[path = "../source_test.rs"]
mod test;
