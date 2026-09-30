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
/// The one block trace method this chain requires; see `docs/design/021`.
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

#[cfg(test)]
mod tests {
    use base::{BlockHash, BlockParent};
    use futures_executor::block_on;

    use super::*;
    use crate::native::rpc::script::{Reply, Script, Scripted};

    const BLOCK_HASH: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PARENT_HASH: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const TX_HASH: &str = "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const TX_HASH_2: &str = "0xdddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    const FROM: &str = "0x1111111111111111111111111111111111111111";
    const TO: &str = "0x2222222222222222222222222222222222222222";

    fn block(transactions: Vec<Value>) -> Value {
        json!({
            "hash": BLOCK_HASH,
            "parentHash": PARENT_HASH,
            "sha3Uncles": "0xdddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            "miner": "0x0000000000000000000000000000000000000000",
            "stateRoot": "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "transactionsRoot":
                "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "receiptsRoot": "0xabababababababababababababababababababababababababababababababab",
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "difficulty": "0x0",
            "number": "0xa",
            "gasLimit": "0x1c9c380",
            "gasUsed": "0x5208",
            "timestamp": "0x64",
            "extraData": "0x",
            "mixHash": "0xcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
            "nonce": "0x0000000000000000",
            "uncles": [],
            "transactions": transactions
        })
    }

    fn transaction(hash: &str, index: u64) -> Value {
        json!({
            "hash": hash, "from": FROM, "to": TO, "value": "0x1",
            "transactionIndex": format!("0x{index:x}"),
            "blockHash": BLOCK_HASH, "blockNumber": "0xa"
        })
    }

    /// The prestate diff of one `FROM -> TO` transfer of 1 wei at 2 wei per
    /// gas: the sender pays 42,001 wei in all and the recipient receives 1.
    fn trace(hashes: &[&str]) -> Value {
        Value::Array(
            hashes
                .iter()
                .map(|hash| {
                    json!({"txHash": hash, "result": {
                        "pre": {FROM: {"balance": "0xa411"}, TO: {"balance": "0x0"}},
                        "post": {FROM: {"balance": "0x0"}, TO: {"balance": "0x1"}},
                    }})
                })
                .collect(),
        )
    }

    fn traced(hashes: &[&str]) -> Reply {
        Reply::ok(TRACE_METHOD, trace(hashes)).expecting(json!([
            BLOCK_HASH,
            {"tracer": "prestateTracer", "tracerConfig": {"diffMode": true}}
        ]))
    }

    fn receipt(hash: &str, index: u64, evidence_number: &str) -> Vec<u8> {
        format!(
            r#"{{
  "logs" : [ ],
  "evidenceNumber" : {evidence_number},
  "effectiveGasPrice" : "0x2",
  "gasUsed" : "0x5208",
  "status" : "0x1",
  "contractAddress" : null,
  "to" : "{TO}",
  "from" : "{FROM}",
  "blockNumber" : "0xa",
  "blockHash" : "{BLOCK_HASH}",
  "transactionIndex" : "0x{index:x}",
  "transactionHash" : "{hash}"
}}"#,
        )
        .into_bytes()
    }

    #[test]
    fn falls_back_to_batched_receipts_once_and_keeps_exact_bytes() {
        let full = serde_json::to_vec_pretty(&block(vec![transaction(TX_HASH, 0)])).unwrap();
        let header = block(vec![Value::String(TX_HASH.to_owned())]);
        let script = Script::node(vec![
            Reply::raw("eth_getBlockByNumber", full.clone()).expecting(json!(["0xa", true])),
            Reply::failure("eth_getBlockReceipts", -32_601, "method not found"),
            Reply::raw("eth_getTransactionReceipt", receipt(TX_HASH, 0, "1")),
            traced(&[TX_HASH]),
            Reply::ok("eth_getBlockByNumber", header).expecting(json!(["0xa", false])),
            Reply::raw("eth_getBlockByNumber", full.clone()),
            Reply::raw("eth_getTransactionReceipt", receipt(TX_HASH, 0, "2")),
            traced(&[TX_HASH]),
        ]);
        let rpc = script.client();
        let raw = block_on(rpc.block(BlockPosition(10))).unwrap().unwrap();
        assert_eq!(raw.reference().hash, BlockHash(vec![0xaa; 32]));
        assert_eq!(raw.reference().height, BlockHeight(10));
        assert_eq!(
            raw.reference().parent,
            Some(BlockParent {
                position: BlockPosition(9),
                hash: BlockHash(vec![0xbb; 32]),
            })
        );
        assert_eq!(raw.block(), full.as_slice());
        assert_eq!(raw.receipts(), [receipt(TX_HASH, 0, "1")]);
        assert_eq!(
            raw.traces(),
            serde_json::to_vec(&trace(&[TX_HASH])).unwrap()
        );
        assert_eq!(
            block_on(rpc.canonical_at(BlockPosition(10)))
                .unwrap()
                .map(|reference| reference.hash),
            Some(BlockHash(vec![0xaa; 32]))
        );
        block_on(rpc.block(BlockPosition(10))).unwrap().unwrap();
        script.exhausted();
    }

    #[test]
    fn block_receipt_method_preserves_each_exact_receipt_result() {
        let raw_receipt = receipt(TX_HASH, 0, "6.0200e+02");
        let array = [b"[\n  ".as_slice(), &raw_receipt, b"\n]".as_slice()].concat();
        let script = Script::node(vec![
            Reply::ok("eth_getBlockByNumber", block(vec![transaction(TX_HASH, 0)])),
            Reply::raw("eth_getBlockReceipts", array).expecting(json!([BLOCK_HASH])),
            traced(&[TX_HASH]),
        ]);
        let raw = block_on(script.client().block(BlockPosition(10)))
            .unwrap()
            .unwrap();
        assert_eq!(raw.receipts(), [raw_receipt]);
    }

    #[test]
    fn batched_receipt_fallback_restores_transaction_order() {
        let first = receipt(TX_HASH, 0, "4.20e+01");
        let second = receipt(TX_HASH_2, 1, "2.00e0");
        let script = Script::node(vec![
            Reply::ok(
                "eth_getBlockByNumber",
                block(vec![transaction(TX_HASH, 0), transaction(TX_HASH_2, 1)]),
            ),
            Reply::failure("eth_getBlockReceipts", -32_601, "method not found"),
            Reply::raw("eth_getTransactionReceipt", first.clone()).expecting(json!([TX_HASH])),
            Reply::raw("eth_getTransactionReceipt", second.clone()).expecting(json!([TX_HASH_2])),
            traced(&[TX_HASH, TX_HASH_2]),
        ]);
        let raw = block_on(script.client().block(BlockPosition(10)))
            .unwrap()
            .unwrap();
        assert_eq!(raw.receipts(), [first, second]);
    }

    /// The startup probe traces the node's own head, so it refuses both a node
    /// that does not implement the method and one that answers it without the
    /// `diffMode` prestate this parser reads.
    #[test]
    fn the_trace_probe_refuses_a_node_that_cannot_serve_a_prestate_diff() {
        let head = || Reply::ok("eth_getBlockByNumber", block(vec![transaction(TX_HASH, 0)]));
        let script = Script::node(vec![
            head(),
            Reply::failure(TRACE_METHOD, METHOD_NOT_FOUND, "the method does not exist"),
            head(),
            Reply::ok(
                TRACE_METHOD,
                json!([{"txHash": TX_HASH, "result": {"pre": {}}}]),
            ),
            head(),
            traced(&[TX_HASH]),
        ]);
        let rpc = script.client();
        let refused = block_on(rpc.validate_tracing()).unwrap_err();
        assert!(!refused.retryable);
        assert_eq!(
            refused,
            Error::untraceable("the node does not implement it")
        );
        assert!(
            refused
                .message
                .contains("cannot serve debug_traceBlockByHash with the prestateTracer"),
            "{}",
            refused.message
        );
        assert!(
            refused.message.contains("debug namespace"),
            "{}",
            refused.message
        );
        let ignored = block_on(rpc.validate_tracing()).unwrap_err();
        assert_eq!(
            ignored,
            Error::untraceable(
                "Ethereum transaction trace has no `post` state, so it is not a diff"
            )
        );
        block_on(rpc.validate_tracing()).expect("a tracing node must be accepted");
        script.exhausted();
    }

    /// A block with nothing in it is not traced at all, and a trace failure
    /// that is not "method not found" keeps the block-read retry policy.
    #[test]
    fn empty_blocks_skip_the_trace_and_other_trace_failures_stay_retryable() {
        let script = Script::node(vec![
            Reply::ok("eth_getBlockByNumber", block(Vec::new())),
            Reply::ok("eth_getBlockByNumber", block(vec![transaction(TX_HASH, 0)])),
            Reply::raw(
                "eth_getBlockReceipts",
                [b"[".to_vec(), receipt(TX_HASH, 0, "1"), b"]".to_vec()].concat(),
            ),
            Reply::failure(TRACE_METHOD, -32_000, "trace limit secret"),
        ]);
        let rpc = script.client();
        let raw = block_on(rpc.block(BlockPosition(10))).unwrap().unwrap();
        assert_eq!(raw.traces(), b"[]");
        let error = block_on(rpc.block(BlockPosition(10))).unwrap_err();
        assert_eq!(
            error.message,
            "Ethereum JSON-RPC request failed with code -32000"
        );
        assert!(error.retryable);
        script.exhausted();
    }

    #[test]
    fn missing_blocks_and_other_receipt_failures_keep_their_retry_policy() {
        let script = Script::node(vec![
            Reply::raw("eth_getBlockByNumber", b" \nnull\t ".to_vec()),
            Reply::ok("eth_getBlockByNumber", Value::Null),
            Reply::ok("eth_getBlockByNumber", block(vec![transaction(TX_HASH, 0)])),
            Reply::failure("eth_getBlockReceipts", -32_000, "rate limit secret"),
        ]);
        let rpc = script.client();
        assert_eq!(block_on(rpc.block(BlockPosition(10))).unwrap(), None);
        let missing = block_on(rpc.tip()).unwrap_err();
        assert_eq!(missing, Error::missing_block());
        let error = block_on(rpc.block(BlockPosition(10))).unwrap_err();
        assert_eq!(
            error.message,
            "Ethereum JSON-RPC request failed with code -32000"
        );
        assert!(error.retryable);
    }

    #[test]
    fn canonical_lookup_distinguishes_json_null_from_non_null_values() {
        let script = Script::node(vec![
            Reply::raw("eth_getBlockByNumber", b" \nnull\t ".to_vec()),
            Reply::ok("eth_getBlockByNumber", json!("null")),
            Reply::ok("eth_getBlockByNumber", json!({"value": null})),
            Reply::ok("eth_getBlockByNumber", json!(false)),
        ]);
        let rpc = script.client();
        assert_eq!(block_on(rpc.canonical_at(BlockPosition(10))).unwrap(), None);
        for _ in 0..3 {
            let error = block_on(rpc.canonical_at(BlockPosition(10))).unwrap_err();
            assert_eq!(
                error.message,
                "Ethereum block result does not match the RPC block shape"
            );
            assert!(error.retryable);
        }
    }
}
