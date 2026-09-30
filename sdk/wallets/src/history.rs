//! One holding's history on both sides of the confirmed line: the capability a
//! consumer declares, the page it answers with, and the join that answers it.
//!
//! A repository pages its own confirmed rows ([`ConfirmedHistory`]) and knows
//! nothing above its checkpoint; an index holds the blocks above it and answers
//! no query. [`History`] is the read that needs both, so it is declared here,
//! with the consumer that composes them, and [`PendingJoin`] implements it in
//! the order [`crate::pending`] states.
//!
//! What this file adds to a repository's page is read-time and nothing else: a
//! status, a confirmation count, and the uncommitted transactions of the first
//! page. The rows, their order, the cursor and the claim about the range behind
//! them are the repository's and are passed on as they were answered.

use base::{BlockRef, BoxFuture};
use indexing::{
    CanonicalBlock, CanonicalPage, CanonicalStatus, CanonicalTransaction, Completeness,
    ConfirmedHistory, HistoryPosition, HistoryQuery, Holder, IndexError, IndexErrorKind,
    NetworkFee, Page, TransactionRef, ValueMovement,
};

use crate::PendingJoin;

/// A transaction's status as [`History`] reports it.
///
/// `Confirmed` is a fact rather than a threshold: it means the durable store
/// holds the transaction, which it only does for blocks at or below the
/// chain's confirmed line, so it will not revert.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransactionStatus {
    /// In a canonical block above the confirmed line. It may still revert.
    Pending {
        /// The including block.
        block: BlockRef,
        /// Newest known height minus inclusion height, plus one.
        confirmations: u64,
    },
    /// Committed at or below the confirmed line. It will not revert.
    Confirmed {
        /// The including block.
        block: BlockRef,
        /// Newest known height minus inclusion height, plus one.
        confirmations: u64,
    },
    /// Included but failed.
    Failed {
        /// The including block.
        block: BlockRef,
        /// The chain's failure reason, when it reports one.
        reason: Option<String>,
    },
}

/// A history entry: a [`CanonicalTransaction`] with its read-time status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedTransaction {
    /// The chain's transaction identifier, which names the network it was
    /// indexed in.
    pub transaction_id: TransactionRef,
    /// Status with confirmation depth.
    pub status: TransactionStatus,
    /// The movements of the queried asset only.
    pub movements: Vec<ValueMovement>,
    /// The native fee, if any.
    pub fee: Option<NetworkFee>,
}

/// A [`History`] page: matching transactions with confirmation status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionPage {
    /// The confirmed line the page was read at.
    pub checkpoint: Option<BlockRef>,
    /// Matching confirmed transactions, ascending by `(height, transaction
    /// order, transaction id)`. Append-only: nothing already on a page is ever
    /// removed or reordered.
    pub transactions: Vec<ObservedTransaction>,
    /// Matching transactions in blocks above the confirmed line, oldest
    /// first. Populated on the first page only (`after` absent), and bounded
    /// by the chain's confirmation depth.
    pub pending: Vec<ObservedTransaction>,
    /// Where the next page resumes, or `None` when nothing else matches.
    pub next: Option<HistoryPosition>,
    /// How much of the history behind this page the repository can prove it
    /// holds.
    ///
    /// The same claim, in the same words, that [`indexing::Balance`] makes about
    /// a fold, and for the same reason: a page is a read of the rows a
    /// backfill is still inserting *below* it, so `next` being `None` says
    /// nothing about the rows older than the first one here. Without it a
    /// caller paging to the end mid-backfill would hold what reads as a
    /// complete export and is not one (`docs/design/030 §8 (ex-032)`, consumer reads).
    ///
    /// It is [`CanonicalPage::completeness`], unchanged. The repository
    /// answered it from the snapshot the rows came from, and the blocks above
    /// the line add nothing to a claim about the range below it.
    pub completeness: Completeness,
}

/// Reads one holding's transaction history, confirmed and pending.
///
/// The narrow capability every history consumer declares — `apps/server`'s route
/// and Solana's absence proof both hold one `Arc<dyn History>` — and the one
/// thing a [`ConfirmedHistory`] repository cannot satisfy by itself, because
/// `pending` is an index's own memory above the confirmed line.
/// [`PendingJoin`] is what satisfies it.
pub trait History: Send + Sync {
    /// One `page` of `of`'s history in `of`'s own asset.
    ///
    /// `transactions` is confirmed and append-only, so a cursor into it stays
    /// valid however far the checkpoint moves. `pending` carries the
    /// uncommitted transactions and is populated on the first page only.
    fn history<'a>(
        &'a self,
        of: &'a dyn Holder,
        page: Page,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>>;
}

impl<R: ConfirmedHistory> History for PendingJoin<R> {
    fn history<'a>(
        &'a self,
        of: &'a dyn Holder,
        page: Page,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
        Box::pin(async move {
            let request = HistoryQuery::new(of, page);
            request.validate()?;
            let sample = self.pending.sampled()?;
            let page = self.repository.history(request.clone()).await?;
            let pending = sample.above(&page);
            Self::observe(&request, page, &pending)
        })
    }
}

/// The read-time half of a page: which block confirmations count from, which
/// uncommitted transactions belong on it, and what each row's status is.
///
/// All of it is the reader's, not the store's. A repository answers canonical
/// rows at a checkpoint; a confirmation count and a `Pending` status are
/// statements about the line those rows were read at.
impl<R> PendingJoin<R> {
    /// Confirmations count from the newest block anything is known about, so
    /// the number keeps meaning "how deep is this" across the line.
    fn depth_from(page: &CanonicalPage, pending: &[CanonicalBlock]) -> Option<BlockRef> {
        pending
            .last()
            .map(|held| held.block().clone())
            .or_else(|| page.checkpoint.clone())
    }

    fn observe(
        request: &HistoryQuery,
        page: CanonicalPage,
        pending: &[CanonicalBlock],
    ) -> Result<TransactionPage, IndexError> {
        let tip = Self::depth_from(&page, pending);
        let transactions = page
            .transactions
            .into_iter()
            .map(|transaction| Self::observed(transaction, tip.as_ref(), true))
            .collect::<Result<Vec<_>, IndexError>>()?;
        Ok(TransactionPage {
            checkpoint: page.checkpoint,
            transactions,
            pending: Self::pending_page(request, pending, tip.as_ref())?,
            next: page.next,
            completeness: page.completeness,
        })
    }

    /// The uncommitted transactions of the queried address and asset.
    ///
    /// Only the first page carries them. `transactions` is ascending and
    /// append-only, which is what makes its cursor stable forever; pending
    /// rows change with every block, so they would break exactly that.
    fn pending_page(
        request: &HistoryQuery,
        pending: &[CanonicalBlock],
        tip: Option<&BlockRef>,
    ) -> Result<Vec<ObservedTransaction>, IndexError> {
        if request.after.is_some() {
            return Ok(Vec::new());
        }
        pending
            .iter()
            .flat_map(CanonicalBlock::transactions)
            .filter_map(|transaction| request.select(transaction.clone()))
            .map(|transaction| Self::observed(transaction, tip, false))
            .collect()
    }

    fn observed(
        transaction: CanonicalTransaction,
        tip: Option<&BlockRef>,
        committed: bool,
    ) -> Result<ObservedTransaction, IndexError> {
        Ok(ObservedTransaction {
            transaction_id: transaction.transaction_id,
            status: Self::observe_status(transaction.status, tip, committed)?,
            movements: transaction.movements,
            fee: transaction.fee,
        })
    }

    /// `Confirmed` is not a threshold but a fact: a transaction is confirmed
    /// when the durable store holds it, which is when it is at or below the
    /// chain's confirmed line and will not revert.
    fn observe_status(
        status: CanonicalStatus,
        tip: Option<&BlockRef>,
        committed: bool,
    ) -> Result<TransactionStatus, IndexError> {
        let block = match status {
            CanonicalStatus::Included { block } => block,
            CanonicalStatus::Failed { block, reason } => {
                return Ok(TransactionStatus::Failed { block, reason });
            }
        };
        let tip = tip.ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::Store,
                "history exists without a checkpoint",
                false,
            )
        })?;
        let confirmations = tip
            .height
            .0
            .checked_sub(block.height.0)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Store,
                    "history contains a transaction beyond its checkpoint",
                    false,
                )
            })?;
        Ok(if committed {
            TransactionStatus::Confirmed {
                block,
                confirmations,
            }
        } else {
            TransactionStatus::Pending {
                block,
                confirmations,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use indexing::{AssetId, BlockHeight, ChainName, Subject};

    use super::*;
    use crate::pending::tests::{
        Held, Landing, Paged, address, admitted, block, interleaved, network, owner, page, query,
        transaction,
    };

    /// `page` as the join reports it over `pending`.
    fn observe(
        request: &HistoryQuery,
        page: CanonicalPage,
        pending: &[CanonicalBlock],
    ) -> Result<TransactionPage, IndexError> {
        PendingJoin::<Paged>::observe(request, page, pending)
    }

    #[test]
    fn a_committed_row_is_confirmed_and_counts_depth_from_the_page_checkpoint() {
        let result = observe(
            &query(None),
            page(Some(block(3)), vec![transaction(2)]),
            &[],
        )
        .expect("canonical history");
        assert_eq!(
            result.transactions[0].status,
            TransactionStatus::Confirmed {
                block: block(2),
                confirmations: 2,
            }
        );
        assert!(result.pending.is_empty());
    }

    #[test]
    fn depth_counts_from_the_pending_tip_once_blocks_are_held() {
        let pending = [admitted(4, 5, "owner"), admitted(5, 7, "owner")];
        let result = observe(
            &query(None),
            page(Some(block(3)), vec![transaction(2)]),
            &pending,
        )
        .expect("folded history");
        assert_eq!(
            result.transactions[0].status,
            TransactionStatus::Confirmed {
                block: block(2),
                confirmations: 4,
            }
        );
        let pending_status = result
            .pending
            .iter()
            .map(|entry| entry.status.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            pending_status,
            [
                TransactionStatus::Pending {
                    block: block(4),
                    confirmations: 2,
                },
                TransactionStatus::Pending {
                    block: block(5),
                    confirmations: 1,
                },
            ]
        );
        assert_eq!(result.checkpoint, Some(block(3)));
    }

    #[test]
    fn later_pages_carry_no_pending_rows_so_the_ascending_cursor_stays_stable() {
        let after = indexing::HistoryPosition {
            height: BlockHeight(2),
            transaction_order: 0,
            transaction: TransactionRef::new(network(), "tx"),
        };
        let result = observe(
            &query(Some(after)),
            page(Some(block(3)), Vec::new()),
            &[admitted(4, 5, "owner")],
        )
        .expect("second page");
        assert!(result.pending.is_empty());
    }

    #[test]
    fn observation_rejects_missing_checkpoint_and_invalid_confirmation_arithmetic() {
        for (checkpoint, height, message) in [
            (None, 0, "history exists without a checkpoint"),
            (
                Some(block(2)),
                3,
                "history contains a transaction beyond its checkpoint",
            ),
            (
                Some(block(u64::MAX)),
                0,
                "history contains a transaction beyond its checkpoint",
            ),
        ] {
            let error = observe(
                &query(None),
                page(checkpoint, vec![transaction(height)]),
                &[],
            )
            .expect_err("invalid included history");
            assert_eq!(error.kind, IndexErrorKind::Store);
            assert_eq!(error.message, message);
            assert!(!error.retryable);
        }
    }

    #[test]
    fn failed_history_keeps_its_existing_passthrough_semantics() {
        let mut failed = transaction(2);
        failed.status = CanonicalStatus::Failed {
            block: block(2),
            reason: Some("reverted".into()),
        };
        let observed =
            observe(&query(None), page(None, vec![failed]), &[]).expect("failed observation");
        assert_eq!(
            observed.transactions[0].status,
            TransactionStatus::Failed {
                block: block(2),
                reason: Some("reverted".into())
            }
        );
        assert_eq!(observed.checkpoint, None);
        assert_eq!(observed.next, None);
    }

    #[test]
    fn history_rejects_an_asset_of_another_chain() {
        let foreign = Subject {
            address: address("owner"),
            asset: AssetId::native(ChainName::new("other")),
        };
        let join = PendingJoin::new(Paged(page(None, Vec::new())), Arc::new(Held::default()));
        let error = futures_executor::block_on(join.history(&foreign, Page::first(10)))
            .expect_err("an asset of another chain");
        assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
    }

    /// And the same transaction is reported once, as confirmed or as pending,
    /// never twice and never not at all.
    #[test]
    fn a_commit_landing_inside_a_read_reports_its_transaction_exactly_once() {
        for landing in Landing::ALL {
            let join = interleaved(landing);
            let page = futures_executor::block_on(join.history(&owner(), Page::first(10)))
                .expect("history");

            let reported = page
                .transactions
                .iter()
                .chain(&page.pending)
                .filter(|entry| entry.transaction_id.id.to_string() == "tx-4")
                .count();
            assert_eq!(
                reported, 1,
                "{landing:?}: the committing transaction is reported once"
            );
        }
    }
}
