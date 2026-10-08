//! Block, receipt, and trace reads for indexing, returning retained
//! [`RawBlock`] evidence.

use std::sync::atomic::Ordering;

use alloy_primitives::hex;
use base::{BlockHeight, BlockPosition, BlockRef};
use json_rpc::{CallError, RawJson};
use serde_json::{Value, json, value::RawValue};

use super::Client;
use crate::native::{
    Error,
    block::{Head, RawBlock, Trace},
};

/// Receipt method not yet known.
pub(super) const RECEIPTS_UNKNOWN: u8 = 0;
/// The node serves `eth_getBlockReceipts`.
const RECEIPTS_BY_BLOCK: u8 = 1;
/// The node lacks it; receipts come from one batched call per transaction.
const RECEIPTS_BY_TRANSACTION: u8 = 2;
/// JSON-RPC "method not found".
const METHOD_NOT_FOUND: i64 = -32_601;
/// Required to recover internal native movements absent from transaction fields and logs.
const TRACE_METHOD: &str = "debug_traceBlockByHash";
/// `eth_getBlockByNumber` tag for the newest produced block.
const LATEST: &str = "latest";
/// `eth_getBlockByNumber` tag for the newest block consensus finalized.
const FINALIZED: &str = "finalized";

impl Error {
    /// A block the node does not currently expose; retry later.
    pub(crate) fn missing_block() -> Self {
        Self::rpc(
            "Ethereum RPC does not currently expose the requested block",
            true,
        )
    }

    /// A node that serves no finalized block yet; retry later.
    pub(crate) fn unfinalized() -> Self {
        Self::rpc(
            "Ethereum RPC serves no finalized block yet, so nothing can be \
             committed as irreversible",
            true,
        )
    }

    /// Evidence from the node that failed validation; retry later.
    fn unusable(self) -> Self {
        Self::rpc(self.message, true)
    }

    /// The node cannot serve block traces, so contract-initiated ether would
    /// be indexed as missing. Never retried: it is a property of the node.
    fn untraceable(reason: &str) -> Self {
        Self::rpc(
            format!(
                "Ethereum node cannot serve {TRACE_METHOD} with the prestateTracer in \
                 diffMode ({reason}), so contract-initiated ether cannot be indexed; \
                 configure an endpoint whose debug namespace and prestate tracer are \
                 enabled, with state available for the blocks being indexed"
            ),
            false,
        )
    }

    /// A failed trace call: an unserved method is terminal and actionable,
    /// anything else keeps the block-read retry policy.
    fn from_trace_call(error: &CallError) -> Self {
        if error.remote_code() == Some(METHOD_NOT_FOUND) {
            return Self::untraceable("the node does not implement it");
        }
        Self::from_block_call(error)
    }
}

impl<C: json_rpc::Client> Client<C> {
    /// The reference of the latest block.
    ///
    /// # Errors
    ///
    /// A retryable [`Error`] while the node exposes no latest block.
    pub async fn tip(&self) -> Result<BlockRef, Error> {
        let raw = self.block_object(LATEST.to_owned(), false).await?;
        let raw = raw.ok_or_else(Error::missing_block)?;
        Ok(Self::head(&raw, None, false)?.reference)
    }

    /// The reference of the newest finalized block.
    ///
    /// This is the consensus layer's own statement, not a depth heuristic:
    /// reverting a finalized epoch requires at least a third of all staked
    /// ether to be slashed. A node that is still syncing, or one running a
    /// pre-merge or single-node development chain, serves no finalized block
    /// at all; that is retryable, because it is a property of the node's
    /// current state rather than of the chain.
    ///
    /// # Errors
    ///
    /// A retryable [`Error`] while the node exposes no finalized block.
    pub async fn confirmed_tip(&self) -> Result<BlockRef, Error> {
        let raw = self.block_object(FINALIZED.to_owned(), false).await?;
        let raw = raw.ok_or_else(Error::unfinalized)?;
        Ok(Self::head(&raw, None, false)?.reference)
    }

    /// The canonical reference at `position`, or `None` past the tip.
    ///
    /// # Errors
    ///
    /// When the node fails or returns an invalid block.
    pub async fn canonical_at(&self, position: BlockPosition) -> Result<Option<BlockRef>, Error> {
        let Some(raw) = self.block_object(Self::tag(position), false).await? else {
            return Ok(None);
        };
        let head = Self::head(&raw, Some(BlockHeight(position.0)), false)?;
        Ok(Some(head.reference))
    }

    /// The complete block at `position` with every receipt, validated by
    /// [`RawBlock::parse`]; `None` while the node does not expose it.
    ///
    /// # Errors
    ///
    /// When the node fails or returns evidence that does not parse. Invalid
    /// evidence is retryable: a lagging node may still be catching up.
    pub async fn block(&self, position: BlockPosition) -> Result<Option<RawBlock>, Error> {
        let Some(raw) = self.block_object(Self::tag(position), true).await? else {
            return Ok(None);
        };
        let head = Self::head(&raw, Some(BlockHeight(position.0)), true)?;
        let receipts = self.receipts(&head).await?;
        let traces = self.traces(&head).await?;
        let network = self.network().clone();
        let block = RawBlock::new(head.reference, network, raw.into_bytes(), receipts, traces);
        block.parse().map_err(Error::unusable)?;
        Ok(Some(block))
    }

    /// Checks that the node answers `debug_traceBlockByHash` for its own head
    /// with a `prestateTracer` diff this parser accepts.
    ///
    /// The composition root calls it beside the chain-ID and token checks, so
    /// a node that cannot trace refuses to start instead of indexing ether
    /// movement it cannot see. The head is traced for real: a probe that only
    /// asked whether the method exists would still accept a node that ignores
    /// `diffMode`.
    ///
    /// # Errors
    ///
    /// A terminal [`Error`] naming the method and the fix when the node does
    /// not serve it, or a retryable one while the node exposes no head.
    pub async fn validate_tracing(&self) -> Result<(), Error> {
        let raw = self.block_object("latest".to_owned(), true).await?;
        let head = Self::head(&raw.ok_or_else(Error::missing_block)?, None, true)?;
        let raw = self.trace(&head).await?;
        Trace::parse_all(&raw, &head).map_err(|error| Error::untraceable(&error.message))?;
        Ok(())
    }

    /// The block's prestate diff, or an empty list when it holds nothing to
    /// trace. One call per block: a per-transaction trace would multiply a
    /// method providers already weigh at 50-300 plain calls.
    async fn traces(&self, head: &Head) -> Result<Vec<u8>, Error> {
        if head.is_empty() {
            return Ok(b"[]".to_vec());
        }
        self.trace(head).await
    }

    async fn trace(&self, head: &Head) -> Result<Vec<u8>, Error> {
        let hash = hex::encode_prefixed(&head.reference.hash.0);
        let parameters = json!([hash, {
            "tracer": "prestateTracer",
            "tracerConfig": {"diffMode": true},
        }]);
        let raw = self
            .call(TRACE_METHOD, parameters)
            .await
            .map_err(|error| Error::from_trace_call(&error))?;
        Ok(raw.into_bytes())
    }

    fn tag(position: BlockPosition) -> String {
        format!("0x{:x}", position.0)
    }

    fn head(raw: &RawJson, height: Option<BlockHeight>, full: bool) -> Result<Head, Error> {
        Head::parse(raw.as_bytes(), height, full).map_err(Error::unusable)
    }

    /// `eth_getBlockByNumber`, or `None` for a JSON `null` result.
    async fn block_object(&self, tag: String, full: bool) -> Result<Option<RawJson>, Error> {
        let raw = self
            .call("eth_getBlockByNumber", json!([tag, full]))
            .await
            .map_err(|error| Error::from_block_call(&error))?;
        Ok((!raw.deserialize::<Value>()?.is_null()).then_some(raw))
    }

    async fn receipts(&self, head: &Head) -> Result<Vec<Vec<u8>>, Error> {
        if head.is_empty() {
            return Ok(Vec::new());
        }
        let mode = self.receipt_mode();
        if mode.load(Ordering::Acquire) != RECEIPTS_BY_TRANSACTION {
            match self.block_receipts(head).await {
                Ok(receipts) => {
                    mode.store(RECEIPTS_BY_BLOCK, Ordering::Release);
                    return receipts;
                }
                Err(error) if error.remote_code() == Some(METHOD_NOT_FOUND) => {
                    mode.store(RECEIPTS_BY_TRANSACTION, Ordering::Release);
                }
                Err(error) => return Err(Error::from_block_call(&error)),
            }
        }
        self.transaction_receipts(head).await
    }

    /// One `eth_getBlockReceipts` call; the outer error keeps the remote code.
    async fn block_receipts(&self, head: &Head) -> Result<Result<Vec<Vec<u8>>, Error>, CallError> {
        let hash = hex::encode_prefixed(&head.reference.hash.0);
        let raw = self.call("eth_getBlockReceipts", json!([hash])).await?;
        Ok(raw
            .deserialize::<Vec<Box<RawValue>>>()
            .map(|values| {
                values
                    .into_iter()
                    .map(|value| value.get().as_bytes().to_vec())
                    .collect()
            })
            .map_err(Error::from))
    }

    async fn transaction_receipts(&self, head: &Head) -> Result<Vec<Vec<u8>>, Error> {
        let calls = head
            .ids()
            .map(|id| ("eth_getTransactionReceipt", json!([id.to_string()])))
            .collect();
        self.batch(calls)
            .await?
            .into_iter()
            .map(|result| {
                let raw = result
                    .map_err(|failure| Error::from_block_call(&CallError::Remote(failure)))?;
                if raw.deserialize::<Value>()?.is_null() {
                    return Err(Error::rpc(
                        "Ethereum transaction receipt is temporarily unavailable",
                        true,
                    ));
                }
                Ok(raw.into_bytes())
            })
            .collect()
    }
}
