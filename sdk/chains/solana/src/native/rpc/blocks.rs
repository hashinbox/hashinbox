//! Finalized block reads over sparse slots.
//!
//! Solana skips slots, so a range of positions holds any number of produced
//! blocks. Every read is one bounded pass: it enumerates produced slots
//! with `getBlocks`, fetches each with `getBlock`, and proves with
//! `getFirstAvailableBlock` before and after that nothing it relies on was
//! pruned meanwhile.

use std::{future::Future, time::Duration};

use base::{BlockPosition, BlockRef};
use serde_json::{json, value::RawValue};
use tokio::time::{Instant, timeout_at};

use super::{Client, Commitment};
use crate::native::{Error, ErrorKind, block::RawBlock};

/// Maximum numeric slot span accepted by one finalized enumeration call.
pub(crate) const MAX_ENUMERATION_SPAN: u64 = 500_000;
const PASS_DEADLINE: Duration = Duration::from_secs(30);
const DESCENDING_WINDOW: u64 = 10_000;
const MAX_ENUMERATIONS: u8 = 64;
const MAX_FORWARD_WINDOW: u64 = 500_000;

impl<C: json_rpc::Client> Client<C> {
    /// The newest retained finalized block.
    ///
    /// # Errors
    ///
    /// A retryable [`ErrorKind::RpcUnavailable`] when the node, its
    /// retention window, or the 30-second pass deadline gets in the way.
    pub async fn tip(&self) -> Result<BlockRef, Error> {
        let tip = Pass::new(self).complete_tip().await?;
        Ok(tip.block.reference().clone())
    }

    /// Up to `limit` produced blocks with positions in `start..=end`, in
    /// order and each connected to the previous one.
    ///
    /// # Errors
    ///
    /// A non-retryable [`ErrorKind::RpcUnavailable`] when `start` was pruned;
    /// otherwise as [`Client::tip`].
    pub async fn blocks(
        &self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> Result<Vec<RawBlock>, Error> {
        Pass::new(self).sparse_blocks(start, end, limit).await
    }

    /// The finalized block at `position`; `None` for a skipped slot.
    ///
    /// # Errors
    ///
    /// As [`Client::blocks`].
    pub async fn canonical_at(&self, position: BlockPosition) -> Result<Option<BlockRef>, Error> {
        let block = Pass::new(self).canonical(position).await?;
        Ok(block.map(|value| value.reference().clone()))
    }

    /// The finalized block at `position` as retained evidence, without
    /// enumeration or retention checks; `None` when the node returns none.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::RpcUnavailable`], also for a block whose header fails
    /// validation.
    pub async fn block(&self, position: BlockPosition) -> Result<Option<RawBlock>, Error> {
        let Some(raw) = self.finalized_block(position.0).await? else {
            return Ok(None);
        };
        RawBlock::new(position, raw.get().as_bytes().to_vec())
            .map(Some)
            .map_err(Error::unusable)
    }

    pub(crate) async fn first_available_block(&self) -> Result<u64, Error> {
        self.request("getFirstAvailableBlock", json!([])).await
    }

    pub(crate) async fn finalized_blocks(
        &self,
        start: u64,
        end: u64,
        min_slot: u64,
    ) -> Result<Vec<u64>, Error> {
        let span = end
            .checked_sub(start)
            .and_then(|difference| difference.checked_add(1))
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidConfiguration,
                    "Solana finalized block range must be ordered",
                )
            })?;
        if span > MAX_ENUMERATION_SPAN {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Solana finalized block range exceeds 500000 slots",
            ));
        }
        let slots = self
            .request::<Vec<u64>>(
                "getBlocks",
                json!([start, end, Commitment::Finalized.config(Some(min_slot))]),
            )
            .await?;
        if slots.iter().any(|slot| *slot < start || *slot > end)
            || slots.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(Error::rpc(
                "Solana getBlocks returned unordered, duplicate, or out-of-range slots",
                true,
            ));
        }
        Ok(slots)
    }

    pub(crate) async fn finalized_block(&self, slot: u64) -> Result<Option<Box<RawValue>>, Error> {
        self.request(
            "getBlock",
            json!([slot, {
                "commitment": "finalized",
                "encoding": "json",
                "transactionDetails": "full",
                "maxSupportedTransactionVersion": 0,
                "rewards": false,
            }]),
        )
        .await
    }
}

/// One bounded source read: a deadline and an enumeration budget.
struct Pass<'a, C> {
    rpc: &'a Client<C>,
    deadline: Instant,
    enumerations: u8,
}

/// The newest produced block and the retention bound proved after reading it.
struct Tip {
    block: RawBlock,
    lower_bound: u64,
}

impl<'a, C: json_rpc::Client> Pass<'a, C> {
    fn new(rpc: &'a Client<C>) -> Self {
        Self {
            rpc,
            deadline: Instant::now() + PASS_DEADLINE,
            enumerations: 0,
        }
    }

    fn ensure_before_deadline(&self, now: Instant) -> Result<(), Error> {
        if now >= self.deadline {
            return Err(Error::rpc(
                "Solana source exceeded its 30-second deadline",
                true,
            ));
        }
        Ok(())
    }

    async fn complete_tip(&mut self) -> Result<Tip, Error> {
        let tip = self.finalized_slot().await?;
        let opening = self.first_available().await?;
        if opening > tip {
            return Err(Error::rpc(
                "Solana first available block is above the finalized slot",
                true,
            ));
        }
        let mut end = tip;
        let candidate = loop {
            let start = end.saturating_sub(DESCENDING_WINDOW - 1).max(opening);
            let slots = self.enumerate(start, end, tip).await?;
            if let Some(candidate) = slots.last().copied() {
                break candidate;
            }
            if start == opening {
                return Err(Error::rpc(
                    "Solana finalized history contains no retained produced block",
                    true,
                ));
            }
            end = start - 1;
        };
        let block = self.required_block(candidate).await?;
        let closing = self.first_available().await?;
        if closing > candidate {
            return Err(Error::rpc(
                "Solana selected anchor was pruned during source acquisition",
                true,
            ));
        }
        Ok(Tip {
            block,
            lower_bound: closing,
        })
    }

    async fn sparse_blocks(
        &mut self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> Result<Vec<RawBlock>, Error> {
        let tip = self.complete_tip().await?;
        let tip_position = tip.block.reference().position.0;
        if start.0 > tip_position {
            return Ok(Vec::new());
        }
        if tip.lower_bound > start.0 {
            return Err(Self::pruned());
        }
        let bounded_end = end.0.min(tip_position);
        let mut cursor = start.0;
        let mut blocks = Vec::with_capacity(limit.min(64));
        while cursor <= bounded_end && blocks.len() < limit {
            self.ensure_before_deadline(Instant::now())?;
            let remaining = limit - blocks.len();
            let (window_end, slots) = self
                .forward_window(cursor, bounded_end, remaining, tip_position)
                .await?;
            self.extend_blocks(slots.into_iter().take(remaining), &mut blocks)
                .await?;
            if blocks.len() == limit || window_end == bounded_end {
                break;
            }
            cursor = window_end
                .checked_add(1)
                .ok_or_else(|| Error::rpc("Solana slot range cannot advance", true))?;
        }
        if self.first_available().await? > start.0 {
            return Err(Self::pruned());
        }
        Ok(blocks)
    }

    async fn forward_window(
        &mut self,
        start: u64,
        end: u64,
        remaining: usize,
        tip: u64,
    ) -> Result<(u64, Vec<u64>), Error> {
        let width = u64::try_from(remaining)
            .unwrap_or(u64::MAX)
            .clamp(DESCENDING_WINDOW, MAX_FORWARD_WINDOW);
        let window_end = start.saturating_add(width - 1).min(end);
        let slots = self.enumerate(start, window_end, tip).await?;
        if window_end == tip && slots.last().copied() != Some(window_end) {
            return Err(Error::rpc(
                "Solana range enumeration omitted its previously proved tip",
                true,
            ));
        }
        Ok((window_end, slots))
    }

    async fn extend_blocks(
        &self,
        slots: impl Iterator<Item = u64>,
        blocks: &mut Vec<RawBlock>,
    ) -> Result<(), Error> {
        for slot in slots {
            let block = self.required_block(slot).await?;
            if let Some(previous) = blocks.last() {
                block.follows(previous)?;
            }
            blocks.push(block);
        }
        Ok(())
    }

    async fn canonical(&mut self, position: BlockPosition) -> Result<Option<RawBlock>, Error> {
        let tip = self.finalized_slot().await?;
        if self.first_available().await? > position.0 {
            return Err(Self::pruned());
        }
        if position.0 > tip {
            return Err(Error::rpc(
                "Solana canonical position is above the finalized slot",
                true,
            ));
        }
        let slots = self.enumerate(position.0, position.0, tip).await?;
        let block = if slots.is_empty() {
            if tip <= position.0 {
                return Err(Error::rpc(
                    "Solana same-slot omission is not canonical evidence",
                    true,
                ));
            }
            None
        } else {
            Some(self.required_block(position.0).await?)
        };
        if self.first_available().await? > position.0 {
            return Err(Self::pruned());
        }
        Ok(block)
    }

    async fn finalized_slot(&self) -> Result<u64, Error> {
        self.within(self.rpc.slot(Commitment::Finalized, None))
            .await
    }

    async fn first_available(&self) -> Result<u64, Error> {
        self.within(self.rpc.first_available_block()).await
    }

    async fn required_block(&self, slot: u64) -> Result<RawBlock, Error> {
        let raw = self
            .within(self.rpc.finalized_block(slot))
            .await?
            .ok_or_else(|| {
                Error::rpc("Solana selected finalized block became unavailable", true)
            })?;
        RawBlock::new(BlockPosition(slot), raw.get().as_bytes().to_vec()).map_err(Error::unusable)
    }

    async fn enumerate(&mut self, start: u64, end: u64, min_slot: u64) -> Result<Vec<u64>, Error> {
        self.ensure_before_deadline(Instant::now())?;
        if self.enumerations >= MAX_ENUMERATIONS {
            return Err(Error::rpc(
                "Solana source exhausted its 64-call enumeration budget",
                true,
            ));
        }
        self.enumerations += 1;
        self.within(self.rpc.finalized_blocks(start, end, min_slot))
            .await
    }

    /// `future` within the pass deadline; its own errors pass unchanged.
    async fn within<T>(&self, future: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
        timeout_at(self.deadline, future)
            .await
            .map_err(|_| Error::rpc("Solana source exceeded its 30-second deadline", true))?
    }

    /// Retention moved past a position this read depends on: retrying the
    /// same read cannot succeed.
    fn pruned() -> Error {
        Error::rpc(
            "Solana required position was pruned during source acquisition",
            false,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use base::BlockHeight;
    use serde_json::{Value, json};
    use solana_hash::Hash;

    use super::*;
    use crate::native::rpc::script::{Script, Scripted};

    fn raw(slot: u64, value: Value) -> RawBlock {
        RawBlock::new(BlockPosition(slot), serde_json::to_vec(&value).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn uses_exact_finalized_indexing_shapes() {
        let raw_block = json!({
            "blockhash": "11111111111111111111111111111111",
            "previousBlockhash": "11111111111111111111111111111111",
            "parentSlot": 6,
            "transactions": [],
            "blockTime": 1,
            "blockHeight": 4,
        });
        let rpc = Script::exchanges([
            ("getFirstAvailableBlock", json!([]), json!(2)),
            (
                "getBlocks",
                json!([2, 7, {
                    "commitment": "finalized",
                    "minContextSlot": 9,
                }]),
                json!([2, 4, 7]),
            ),
            (
                "getBlock",
                json!([7, {
                    "commitment": "finalized",
                    "encoding": "json",
                    "transactionDetails": "full",
                    "maxSupportedTransactionVersion": 0,
                    "rewards": false,
                }]),
                raw_block.clone(),
            ),
        ]);
        let client = rpc.client();

        assert_eq!(client.first_available_block().await.unwrap(), 2);
        assert_eq!(client.finalized_blocks(2, 7, 9).await.unwrap(), [2, 4, 7]);
        let block = client.finalized_block(7).await.unwrap().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(block.get()).unwrap(),
            raw_block
        );
        rpc.exhausted();
    }

    #[tokio::test]
    async fn rejects_invalid_request_bounds_before_rpc() {
        let rpc = Script::exchanges([]);
        let client = rpc.client();
        for (start, end, message) in [
            (2, 1, "Solana finalized block range must be ordered"),
            (u64::MAX, 0, "Solana finalized block range must be ordered"),
            (0, u64::MAX, "Solana finalized block range must be ordered"),
            (
                0,
                MAX_ENUMERATION_SPAN,
                "Solana finalized block range exceeds 500000 slots",
            ),
        ] {
            let error = client.finalized_blocks(start, end, 9).await.unwrap_err();
            assert_eq!(error.kind, ErrorKind::InvalidConfiguration);
            assert_eq!(error.message, message);
        }
        rpc.exhausted();
    }

    #[tokio::test]
    async fn accepts_maximum_span_and_final_u64_slot_without_changing_rpc_bounds() {
        for (start, end) in [
            (0, MAX_ENUMERATION_SPAN - 1),
            (u64::MAX, u64::MAX),
            (u64::MAX - MAX_ENUMERATION_SPAN + 1, u64::MAX),
        ] {
            let rpc = Script::exchanges([(
                "getBlocks",
                json!([start, end, { "commitment": "finalized", "minContextSlot": end }]),
                json!([end]),
            )]);
            let client = rpc.client();
            assert_eq!(
                client.finalized_blocks(start, end, end).await.unwrap(),
                [end]
            );
            rpc.exhausted();
        }
    }

    #[tokio::test]
    async fn accepts_empty_and_sparse_slots_without_reordering() {
        for slots in [vec![], vec![2], vec![7], vec![2, 4, 7]] {
            let rpc = Script::exchanges([(
                "getBlocks",
                json!([2, 7, {"commitment":"finalized","minContextSlot":9}]),
                json!(slots),
            )]);
            let client = rpc.client();
            assert_eq!(client.finalized_blocks(2, 7, 9).await.unwrap(), slots);
            rpc.exhausted();
        }
    }

    #[tokio::test]
    async fn rejects_unordered_duplicate_and_out_of_range_slots() {
        for slots in [json!([3, 2]), json!([2, 2]), json!([1]), json!([4])] {
            let rpc = Script::exchanges([(
                "getBlocks",
                json!([2, 3, {
                    "commitment": "finalized",
                    "minContextSlot": 9,
                }]),
                slots,
            )]);
            let client = rpc.client();
            let error = client.finalized_blocks(2, 3, 9).await.unwrap_err();
            assert_eq!(error.kind, ErrorKind::RpcUnavailable);
            assert_eq!(
                error.message,
                "Solana getBlocks returned unordered, duplicate, or out-of-range slots",
            );
            rpc.exhausted();
        }
    }

    fn hash(byte: u8) -> String {
        Hash::new_from_array([byte; 32]).to_string()
    }

    fn block(slot: u64, height: u64, parent_slot: u64, byte: u8, parent: u8) -> Value {
        json!({
            "blockhash": hash(byte),
            "previousBlockhash": hash(parent),
            "parentSlot": parent_slot,
            "transactions": [],
            "blockTime": slot,
            "blockHeight": height,
        })
    }

    fn enumeration(
        start: u64,
        end: u64,
        min_slot: u64,
        values: Value,
    ) -> (&'static str, Value, Value) {
        (
            "getBlocks",
            json!([start, end, {
                "commitment": "finalized",
                "minContextSlot": min_slot,
            }]),
            values,
        )
    }

    fn full(slot: u64, value: Value) -> (&'static str, Value, Value) {
        (
            "getBlock",
            json!([slot, {
                "commitment": "finalized",
                "encoding": "json",
                "transactionDetails": "full",
                "maxSupportedTransactionVersion": 0,
                "rewards": false,
            }]),
            value,
        )
    }

    #[tokio::test]
    async fn range_tip_omission_precedes_fetching_even_when_limit_is_already_available() {
        let rpc = Script::exchanges([
            ("getSlot", json!([{"commitment":"finalized"}]), json!(107)),
            ("getFirstAvailableBlock", json!([]), json!(1)),
            enumeration(1, 107, 107, json!([107])),
            full(107, block(107, 4, 103, 4, 3)),
            ("getFirstAvailableBlock", json!([]), json!(1)),
            enumeration(100, 107, 107, json!([100])),
        ]);
        let source = rpc.client();
        let error = source
            .blocks(BlockPosition(100), BlockPosition(107), 1)
            .await
            .unwrap_err();
        assert_eq!(
            error.message,
            "Solana range enumeration omitted its previously proved tip"
        );
        assert!(error.retryable);
        rpc.exhausted();
    }

    #[tokio::test]
    async fn closing_retention_is_checked_after_fetching_the_requested_block_limit() {
        let rpc = Script::exchanges([
            ("getSlot", json!([{"commitment":"finalized"}]), json!(107)),
            ("getFirstAvailableBlock", json!([]), json!(1)),
            enumeration(1, 107, 107, json!([107])),
            full(107, block(107, 4, 103, 4, 3)),
            ("getFirstAvailableBlock", json!([]), json!(1)),
            enumeration(100, 107, 107, json!([100, 107])),
            full(100, block(100, 2, 1, 2, 1)),
            ("getFirstAvailableBlock", json!([]), json!(101)),
        ]);
        let source = rpc.client();
        let error = source
            .blocks(BlockPosition(100), BlockPosition(107), 1)
            .await
            .unwrap_err();
        assert_eq!(
            error.message,
            "Solana required position was pruned during source acquisition"
        );
        assert!(!error.retryable);
        rpc.exhausted();
    }

    #[tokio::test]
    async fn finds_a_complete_tip_below_empty_finalized_slots() {
        let rpc = Script::exchanges([
            (
                "getSlot",
                json!([{"commitment":"finalized"}]),
                json!(20_005),
            ),
            ("getFirstAvailableBlock", json!([]), json!(1)),
            enumeration(10_006, 20_005, 20_005, json!([])),
            enumeration(6, 10_005, 20_005, json!([])),
            enumeration(1, 5, 20_005, json!([3])),
            full(3, block(3, 2, 1, 3, 1)),
            ("getFirstAvailableBlock", json!([]), json!(1)),
        ]);
        let source = rpc.client();

        let tip = source.tip().await.unwrap();
        assert_eq!(tip.position, BlockPosition(3));
        assert_eq!(tip.height, BlockHeight(2));
        rpc.exhausted();
    }

    #[tokio::test]
    async fn fetches_sparse_ranges_in_order_and_truncates_by_block_count() {
        let rpc = Script::exchanges([
            ("getSlot", json!([{"commitment":"finalized"}]), json!(107)),
            ("getFirstAvailableBlock", json!([]), json!(1)),
            enumeration(1, 107, 107, json!([1, 100, 103, 107])),
            full(107, block(107, 4, 103, 4, 3)),
            ("getFirstAvailableBlock", json!([]), json!(1)),
            enumeration(100, 107, 107, json!([100, 103, 107])),
            full(100, block(100, 2, 1, 2, 1)),
            full(103, block(103, 3, 100, 3, 2)),
            ("getFirstAvailableBlock", json!([]), json!(1)),
        ]);
        let source = rpc.client();

        let blocks = source
            .blocks(BlockPosition(100), BlockPosition(107), 2)
            .await
            .unwrap();
        assert_eq!(
            blocks
                .iter()
                .map(|value| value.reference().position)
                .collect::<Vec<_>>(),
            [BlockPosition(100), BlockPosition(103)]
        );
        rpc.exhausted();
    }

    #[tokio::test]
    async fn returns_empty_above_the_proved_tip_without_range_enumeration() {
        let rpc = Script::exchanges([
            ("getSlot", json!([{"commitment":"finalized"}]), json!(10)),
            ("getFirstAvailableBlock", json!([]), json!(1)),
            enumeration(1, 10, 10, json!([7])),
            full(7, block(7, 2, 4, 2, 1)),
            ("getFirstAvailableBlock", json!([]), json!(1)),
        ]);
        let source = rpc.client();

        assert!(
            source
                .blocks(BlockPosition(8), BlockPosition(20), 5)
                .await
                .unwrap()
                .is_empty()
        );
        rpc.exhausted();
    }

    #[tokio::test]
    async fn rejects_pruning_that_moves_past_the_required_start() {
        let rpc = Script::exchanges([
            ("getSlot", json!([{"commitment":"finalized"}]), json!(107)),
            ("getFirstAvailableBlock", json!([]), json!(1)),
            enumeration(1, 107, 107, json!([100, 107])),
            full(107, block(107, 2, 100, 2, 1)),
            ("getFirstAvailableBlock", json!([]), json!(101)),
        ]);
        let source = rpc.client();

        let error = source
            .blocks(BlockPosition(100), BlockPosition(107), 2)
            .await
            .unwrap_err();
        assert!(!error.retryable);
        assert!(error.message.contains("pruned"));
        rpc.exhausted();
    }

    #[tokio::test]
    async fn proves_omission_only_with_both_pruning_witnesses_and_older_tip() {
        let rpc = Script::exchanges([
            ("getSlot", json!([{"commitment":"finalized"}]), json!(10)),
            ("getFirstAvailableBlock", json!([]), json!(1)),
            enumeration(7, 7, 10, json!([])),
            ("getFirstAvailableBlock", json!([]), json!(1)),
        ]);
        let source = rpc.client();
        assert_eq!(source.canonical_at(BlockPosition(7)).await.unwrap(), None);
        rpc.exhausted();
    }

    #[tokio::test]
    async fn returns_a_changed_complete_canonical_block() {
        let rpc = Script::exchanges([
            ("getSlot", json!([{"commitment":"finalized"}]), json!(10)),
            ("getFirstAvailableBlock", json!([]), json!(1)),
            enumeration(7, 7, 10, json!([7])),
            full(7, block(7, 5, 6, 9, 8)),
            ("getFirstAvailableBlock", json!([]), json!(1)),
        ]);
        let source = rpc.client();

        let canonical = source
            .canonical_at(BlockPosition(7))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(canonical.hash, base::BlockHash([9; 32].to_vec()));
        rpc.exhausted();
    }

    #[tokio::test]
    async fn refuses_same_slot_omission_and_late_pruning() {
        let same_slot = Script::exchanges([
            ("getSlot", json!([{"commitment":"finalized"}]), json!(7)),
            ("getFirstAvailableBlock", json!([]), json!(1)),
            enumeration(7, 7, 7, json!([])),
        ]);
        assert!(
            same_slot
                .client()
                .canonical_at(BlockPosition(7))
                .await
                .unwrap_err()
                .retryable
        );
        same_slot.exhausted();

        let pruned = Script::exchanges([
            ("getSlot", json!([{"commitment":"finalized"}]), json!(10)),
            ("getFirstAvailableBlock", json!([]), json!(1)),
            enumeration(7, 7, 10, json!([])),
            ("getFirstAvailableBlock", json!([]), json!(8)),
        ]);
        let error = pruned
            .client()
            .canonical_at(BlockPosition(7))
            .await
            .unwrap_err();
        assert!(!error.retryable);
        assert_eq!(
            error.message,
            "Solana required position was pruned during source acquisition"
        );
        pruned.exhausted();
    }

    #[tokio::test]
    async fn deadline_and_enumeration_budget_fail_retryably() {
        let rpc = Script::exchanges([]).client();
        let mut expired = Pass {
            rpc: &rpc,
            deadline: Instant::now() - Duration::from_millis(1),
            enumerations: 0,
        };
        let error = expired.enumerate(1, 1, 1).await.unwrap_err();
        assert!(error.retryable);
        assert!(error.message.contains("deadline"));

        let mut exhausted = Pass {
            rpc: &rpc,
            deadline: Instant::now() + Duration::from_secs(1),
            enumerations: MAX_ENUMERATIONS,
        };
        let error = exhausted.enumerate(1, 1, 1).await.unwrap_err();
        assert!(error.retryable);
        assert!(error.message.contains("64-call"));
    }

    #[tokio::test]
    async fn rejects_broken_produced_height_or_parent_sequence() {
        for second in [
            block(103, 4, 100, 3, 2),
            block(103, 3, 99, 3, 2),
            block(103, 3, 100, 3, 9),
        ] {
            let rpc = Script::exchanges([
                ("getSlot", json!([{"commitment":"finalized"}]), json!(103)),
                ("getFirstAvailableBlock", json!([]), json!(1)),
                enumeration(1, 103, 103, json!([1, 100, 103])),
                full(103, second.clone()),
                ("getFirstAvailableBlock", json!([]), json!(1)),
                enumeration(100, 103, 103, json!([100, 103])),
                full(100, block(100, 2, 1, 2, 1)),
                full(103, second),
            ]);
            let error = rpc
                .client()
                .blocks(BlockPosition(100), BlockPosition(103), 2)
                .await
                .unwrap_err();
            assert!(error.message.contains("strict canonical sequence"));
            rpc.exhausted();
        }
    }

    #[tokio::test]
    async fn validates_parent_connections_across_sparse_enumeration_windows() {
        for parent in [100, 99] {
            let second = block(10_103, 3, parent, 3, 2);
            let rpc = Script::exchanges([
                (
                    "getSlot",
                    json!([{"commitment":"finalized"}]),
                    json!(10_103),
                ),
                ("getFirstAvailableBlock", json!([]), json!(1)),
                enumeration(104, 10_103, 10_103, json!([10_103])),
                full(10_103, second.clone()),
                ("getFirstAvailableBlock", json!([]), json!(1)),
                enumeration(100, 10_099, 10_103, json!([100])),
                full(100, block(100, 2, 1, 2, 1)),
                enumeration(10_100, 10_103, 10_103, json!([10_103])),
                full(10_103, second),
                ("getFirstAvailableBlock", json!([]), json!(1)),
            ]);
            let client = rpc.client();
            let result = client
                .clone()
                .blocks(BlockPosition(100), BlockPosition(10_103), 2)
                .await;
            if parent == 100 {
                let blocks = result.expect("connected blocks across windows");
                assert_eq!(blocks.len(), 2);
                assert_eq!(blocks[0].reference().position, BlockPosition(100));
                assert_eq!(blocks[1].reference().position, BlockPosition(10_103));
                assert_eq!(blocks[1].reference().height, BlockHeight(3));
            } else {
                let error = result.expect_err("cross-window parent mismatch");
                assert!(error.retryable);
                assert_eq!(
                    error.message,
                    "Solana produced blocks are not a strict canonical sequence"
                );
                assert_eq!(client.first_available_block().await.unwrap(), 1);
            }
            rpc.exhausted();
        }
    }

    #[tokio::test]
    async fn within_passes_rpc_errors_through_unchanged() {
        let rpc = Script::exchanges([]).client();
        let pass = Pass::new(&rpc);
        for error in [
            Error::new(ErrorKind::InvalidConfiguration, "terminal fixture"),
            Error::rpc("retryable fixture", true),
            Error::rpc("pruned fixture", false),
            Error::malformed("getBlock"),
            Error::below_min_slot(),
        ] {
            let returned = pass
                .within::<()>(std::future::ready(Err(error.clone())))
                .await
                .expect_err("RPC error must remain an error");
            assert_eq!(returned, error);
        }
    }

    #[tokio::test]
    async fn within_preserves_successful_values() {
        let rpc = Script::exchanges([]).client();
        let pass = Pass::new(&rpc);
        let value = pass
            .within(std::future::ready(Ok(vec![3, 100, 107])))
            .await
            .expect("successful RPC must retain its value");

        assert_eq!(value, vec![3, 100, 107]);
    }

    #[tokio::test]
    async fn within_times_out_a_pending_future_at_the_existing_deadline() {
        let rpc = Script::exchanges([]).client();
        let deadline = Instant::now() - Duration::from_millis(1);
        let pass = Pass {
            rpc: &rpc,
            deadline,
            enumerations: 17,
        };
        let value = pass.within(std::future::ready(Ok(42))).await.unwrap();
        assert_eq!(
            value, 42,
            "a ready future retains timeout_at polling precedence"
        );
        let error = pass
            .within(std::future::pending::<Result<(), Error>>())
            .await
            .expect_err("pending RPC must respect the elapsed pass deadline");

        assert_eq!(
            error.message,
            "Solana source exceeded its 30-second deadline"
        );
        assert!(error.retryable);
        assert_eq!(pass.deadline, deadline);
        assert_eq!(pass.enumerations, 17);
    }

    #[test]
    fn deadline_guard_is_strict_and_does_not_reset_attempt_state() {
        let rpc = Script::exchanges([]).client();
        let deadline = Instant::now();
        let pass = Pass {
            rpc: &rpc,
            deadline,
            enumerations: 17,
        };

        assert!(
            pass.ensure_before_deadline(deadline - Duration::from_nanos(1))
                .is_ok()
        );
        for observed in [deadline, deadline + Duration::from_nanos(1)] {
            let error = pass.ensure_before_deadline(observed).unwrap_err();
            assert_eq!(
                error.message,
                "Solana source exceeded its 30-second deadline"
            );
            assert!(error.retryable);
        }
        assert_eq!(pass.deadline, deadline);
        assert_eq!(pass.enumerations, 17);
    }

    #[tokio::test]
    async fn expired_attempt_rejects_before_spending_enumeration_budget_or_rpc() {
        let scripted = Script::exchanges([]);
        let rpc = scripted.client();
        let deadline = Instant::now() - Duration::from_nanos(1);
        for enumerations in [0, MAX_ENUMERATIONS] {
            let mut pass = Pass {
                rpc: &rpc,
                deadline,
                enumerations,
            };
            let error = pass.enumerate(1, 1, 1).await.unwrap_err();
            assert_eq!(
                error.message,
                "Solana source exceeded its 30-second deadline"
            );
            assert!(error.retryable);
            assert_eq!(pass.enumerations, enumerations);
            assert_eq!(pass.deadline, deadline);
        }
        scripted.exhausted();
    }

    #[tokio::test]
    async fn selected_anchor_accepts_retention_equality_but_retries_later_pruning() {
        for closing in [7, 8] {
            let rpc = Script::exchanges([
                ("getSlot", json!([{"commitment":"finalized"}]), json!(10)),
                ("getFirstAvailableBlock", json!([]), json!(1)),
                enumeration(1, 10, 10, json!([7])),
                full(7, block(7, 5, 6, 9, 8)),
                ("getFirstAvailableBlock", json!([]), json!(closing)),
            ]);
            let result = rpc.client().tip().await;
            if closing == 7 {
                assert_eq!(result.unwrap().position, BlockPosition(7));
            } else {
                let error = result.unwrap_err();
                assert!(error.retryable);
                assert_eq!(
                    error.message,
                    "Solana selected anchor was pruned during source acquisition"
                );
            }
            rpc.exhausted();
        }
    }

    #[tokio::test]
    async fn canonical_retention_equality_succeeds_and_opening_pruning_precedes_tip_check() {
        let rpc = Script::exchanges([
            ("getSlot", json!([{"commitment":"finalized"}]), json!(10)),
            ("getFirstAvailableBlock", json!([]), json!(7)),
            enumeration(7, 7, 10, json!([7])),
            full(7, block(7, 5, 6, 9, 8)),
            ("getFirstAvailableBlock", json!([]), json!(7)),
        ]);
        let result = rpc
            .client()
            .canonical_at(BlockPosition(7))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.position, BlockPosition(7));
        rpc.exhausted();

        let pruned = Script::exchanges([
            ("getSlot", json!([{"commitment":"finalized"}]), json!(6)),
            ("getFirstAvailableBlock", json!([]), json!(8)),
        ]);
        let error = pruned
            .client()
            .canonical_at(BlockPosition(7))
            .await
            .unwrap_err();
        assert!(!error.retryable);
        assert_eq!(
            error.message,
            "Solana required position was pruned during source acquisition"
        );
        pruned.exhausted();
    }

    #[test]
    fn produced_height_overflow_precedes_other_connection_errors() {
        let previous = raw(7, block(7, u64::MAX, 6, 7, 6));
        let current = raw(7, block(7, 0, 6, 9, 8));
        let error = current.follows(&previous).unwrap_err();
        assert!(error.retryable);
        assert_eq!(error.message, "Solana produced height is exhausted");
    }
}
