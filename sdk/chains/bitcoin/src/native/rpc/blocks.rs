//! Block reads for indexing, returning retained [`RawBlock`] evidence.

use base::{BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef};
use bitcoin::Transaction as NativeTransaction;
use serde_json::{Value, json};

use super::{
    Client,
    wire::{format_block_hash, parse_block_hash},
};
use crate::native::{
    Error, TransactionId,
    block::{RawBlock, Transaction},
    json::Object,
};

/// `getblockhash`: height out of range.
const HEIGHT_OUT_OF_RANGE: i64 = -8;
/// `getblock` / `getrawtransaction`: no such block or transaction.
const NOT_FOUND: i64 = -5;

impl Error {
    /// A block the node does not currently expose; retry later.
    pub(crate) fn missing_block() -> Self {
        Self::rpc(
            "Bitcoin Core does not currently expose the requested block",
            true,
        )
    }
}

impl<C: json_rpc::Client> Client<C> {
    /// The reference of the best block.
    ///
    /// # Errors
    ///
    /// A retryable [`Error`] when the tip moves while it is read.
    pub async fn tip(&self) -> Result<BlockRef, Error> {
        let height = self.block_count().await?;
        let hash = self.canonical_hash(height).await?.ok_or_else(|| {
            Error::rpc(
                "Bitcoin canonical height changed while its hash was being read",
                true,
            )
        })?;
        self.header(&hash, height).await
    }

    /// The canonical reference at `position`, or `None` past the tip.
    ///
    /// # Errors
    ///
    /// When the node fails or returns an invalid header.
    pub async fn canonical_at(&self, position: BlockPosition) -> Result<Option<BlockRef>, Error> {
        let height = BlockHeight(position.0);
        if height > self.block_count().await? {
            return Ok(None);
        }
        let Some(hash) = self.canonical_hash(height).await? else {
            return Ok(None);
        };
        self.header(&hash, height).await.map(Some)
    }

    /// The canonical hash at `height`; `None` when the height vanished in a
    /// shorter, higher-work reorg.
    ///
    /// # Errors
    ///
    /// When the node fails or returns an invalid hash.
    pub async fn canonical_hash(&self, height: BlockHeight) -> Result<Option<BlockHash>, Error> {
        let raw = self
            .optional("getblockhash", json!([height.0]), HEIGHT_OUT_OF_RANGE)
            .await?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let text: String = raw.deserialize()?;
        parse_block_hash(&text)
            .map(Some)
            .ok_or_else(|| Error::invalid_reply("Bitcoin block hash is invalid"))
    }

    /// The complete block at `position` with every spent output resolved,
    /// validated by [`RawBlock::parse`]; `None` while the node does not
    /// expose it.
    ///
    /// The block is read by hash and the height is read again afterwards, so
    /// a same-height reorg during the read fails instead of mixing blocks.
    ///
    /// # Errors
    ///
    /// When the node fails or returns evidence that does not parse. Invalid
    /// evidence is retryable: a lagging node may still be catching up.
    pub async fn block(&self, position: BlockPosition) -> Result<Option<RawBlock>, Error> {
        let height = BlockHeight(position.0);
        let Some(hash) = self.canonical_hash(height).await? else {
            return Ok(None);
        };
        let params = json!([format_block_hash(&hash)?, 2]);
        let Some(raw) = self.optional("getblock", params, NOT_FOUND).await? else {
            return Ok(None);
        };
        let bytes = self.resolve_prevouts(raw.into_bytes()).await?;
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| Error::invalid_reply("Bitcoin block result is not valid JSON"))?;
        let object = Object::new(&value, "Bitcoin block result", Error::invalid_reply)?;
        let reference = RawBlock::claimed_reference(&object, Some(height), Some(&hash))
            .map_err(Error::unusable)?;
        let block = RawBlock::new(reference, *self.network(), bytes);
        block.parse().map_err(Error::unusable)?;
        if self.canonical_hash(height).await? != Some(hash) {
            return Err(Error::rpc(
                "Bitcoin canonical block changed while it was being fetched",
                true,
            ));
        }
        Ok(Some(block))
    }

    /// The confirmed transaction `id`, decoded and checked against its txid.
    pub(crate) async fn confirmed_transaction(
        &self,
        id: TransactionId,
    ) -> Result<NativeTransaction, Error> {
        let raw = self
            .optional(
                "getrawtransaction",
                json!([id.to_string(), true]),
                NOT_FOUND,
            )
            .await?
            .ok_or_else(|| {
                Error::rpc(
                    format!("Bitcoin previous transaction {id} disappeared during block ingestion"),
                    true,
                )
            })?;
        let value: Value = raw.deserialize()?;
        let object = Object::new(
            &value,
            "Bitcoin getrawtransaction result",
            Error::invalid_reply,
        )?;
        let returned = object
            .string("txid", "Bitcoin previous transaction ID")?
            .parse::<TransactionId>()
            .map_err(|_| object.fail("Bitcoin previous transaction ID is invalid"))?;
        if returned != id {
            return Err(object
                .fail("Bitcoin previous-transaction lookup returned a different transaction ID"));
        }
        let transaction = Transaction::decode(&object, id)?;
        let block = object.string("blockhash", "Bitcoin previous transaction block hash")?;
        parse_block_hash(block).ok_or_else(|| object.fail("Bitcoin block hash is invalid"))?;
        Ok(transaction)
    }

    async fn block_count(&self) -> Result<BlockHeight, Error> {
        let count = self.request("getblockcount", json!([])).await?;
        count
            .as_u64()
            .map(BlockHeight)
            .ok_or_else(|| Error::invalid_reply("Bitcoin block count is invalid"))
    }

    async fn header(&self, hash: &BlockHash, height: BlockHeight) -> Result<BlockRef, Error> {
        let params = json!([format_block_hash(hash)?, true]);
        let value = self.request("getblockheader", params).await?;
        let header = Object::new(
            &value,
            "Bitcoin getblockheader result",
            Error::invalid_reply,
        )?;
        if BlockHeight(header.u64("height", "Bitcoin block-header height")?) != height {
            return Err(header.fail("Bitcoin block header does not match the requested height"));
        }
        let hash_text = header.string("hash", "Bitcoin block-header hash")?;
        let actual = parse_block_hash(hash_text)
            .ok_or_else(|| header.fail("Bitcoin block hash is invalid"))?;
        let parent = match height.0.checked_sub(1) {
            None => None,
            Some(position) => {
                let text = header.string("previousblockhash", "Bitcoin previous block hash")?;
                let hash = parse_block_hash(text)
                    .ok_or_else(|| header.fail("Bitcoin block hash is invalid"))?;
                Some(BlockParent {
                    position: BlockPosition(position),
                    hash,
                })
            }
        };
        let timestamp = header.u64("time", "Bitcoin block-header timestamp")?;
        if &actual != hash {
            return Err(header.fail("Bitcoin header lookup returned a different block hash"));
        }
        Ok(BlockRef {
            position: BlockPosition(height.0),
            height,
            hash: actual,
            parent,
            timestamp: Some(timestamp),
        })
    }
}
