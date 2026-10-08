//! Finalized block reads over sparse slots.
//!
//! Solana skips slots, so a range of positions holds any number of produced
//! blocks. Every read is one bounded acquisition: it enumerates produced slots
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
const ACQUISITION_DEADLINE: Duration = Duration::from_secs(30);
const DESCENDING_WINDOW: u64 = 10_000;
const MAX_ENUMERATIONS: u8 = 64;
const MAX_FORWARD_WINDOW: u64 = 500_000;

impl<C: json_rpc::Client> Client<C> {
    /// The newest retained finalized block.
    ///
    /// # Errors
    ///
    /// A retryable [`ErrorKind::RpcUnavailable`] when the node, its
    /// retention window, or the 30-second acquisition deadline gets in the way.
    pub async fn tip(&self) -> Result<BlockRef, Error> {
        let tip = Acquisition::new(self).complete_tip().await?;
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
        Acquisition::new(self)
            .sparse_blocks(start, end, limit)
            .await
    }

    /// The finalized block at `position`; `None` for a skipped slot.
    ///
    /// # Errors
    ///
    /// As [`Client::blocks`].
    pub async fn canonical_at(&self, position: BlockPosition) -> Result<Option<BlockRef>, Error> {
        let block = Acquisition::new(self).canonical(position).await?;
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
        RawBlock::new(
            position,
            self.network().clone(),
            raw.get().as_bytes().to_vec(),
        )
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

/// One bounded source acquisition: the client it reads through, its
/// deadline, and the enumerations it has spent of its budget.
struct Acquisition<'a, C> {
    rpc: &'a Client<C>,
    deadline: Instant,
    enumerations: u8,
}

/// The newest produced block and the retention bound proved after reading it.
struct Tip {
    block: RawBlock,
    lower_bound: u64,
}

impl<'a, C: json_rpc::Client> Acquisition<'a, C> {
    fn new(rpc: &'a Client<C>) -> Self {
        Self {
            rpc,
            deadline: Instant::now() + ACQUISITION_DEADLINE,
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
        let network = self.rpc.network().clone();
        RawBlock::new(BlockPosition(slot), network, raw.get().as_bytes().to_vec())
            .map_err(Error::unusable)
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

    /// `future` within the deadline; its own errors pass unchanged.
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
