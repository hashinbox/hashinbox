//! What a re-anchor may and may not do to the row it lands on.
//!
//! Split from `watch.rs` by role, for the reason `chunk.rs` and `rollback.rs`
//! state: one file per guarantee under the repository's 500-line cap.
//!
//! **Why this guarantee exists at all.** [`WatchStore::reanchor`] is a
//! postcondition — it states what must be true afterwards, and repeating it is
//! how a caller that lost the acknowledgement reaches a definite state. Once the
//! observation's row also carries what its own indexing has added up to, an
//! operation stated that loosely would be a total reset: re-anchoring a watch
//! would zero a balance nothing asked it to touch. The answer is to narrow the
//! *columns* rather than to add a precondition on existence, so the recovery
//! story survives intact, and the one precondition that is added — a birthday
//! may not go down — still holds under repeat, because raising to a birthday
//! already stored is a no-op.
//!
//! Both halves are asserted here: the refusal, and the fact that a legal
//! re-anchor does not disturb what a commit accumulated.

use base::Decimal;

use indexing::{
    BalanceQuery, BalanceStore, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef,
    CanonicalBlock, FeeSettlement, IndexErrorKind, IndexStore, IndexedBatch, InterpretedBlock,
    MovementId, ObservationDraft, ObservationDraftStatus, OutputChanges, TransactionRef,
    ValueMovement, Watch, WatchProgress, WatchStore,
};

use super::watch::WatchContract;

impl<R: WatchStore + IndexStore> WatchContract<'_, R> {
    /// A re-anchor below the stored birthday is refused, and changes nothing.
    ///
    /// A birthday is the lowest block an observation owes, so lowering one
    /// claims a range that was never inspected: the rows below it are not there,
    /// and the claim this call discards would have been the only evidence that
    /// they were ever looked for. Raising is settling a range nobody will read
    /// ([`indexing::Backfilling::cancel`]), which is the whole of what the
    /// operation is for.
    pub async fn refuses_a_reanchor_below_the_stored_birthday(&self) {
        let watch = self.watch("wallet", "native", "a", 20);
        self.add(vec![watch.clone()]).await;
        let recorded = self.record(std::slice::from_ref(&watch)).await;
        let held = [WatchProgress {
            watch: watch.clone(),
            indexed_through: Some(recorded),
        }];

        let error = self
            .repository
            .reanchor(vec![Watch {
                start: BlockPosition(5),
                ..watch.clone()
            }])
            .await
            .expect_err("a birthday below the stored one");

        assert_eq!(error.kind, IndexErrorKind::Conflict);
        assert_eq!(
            self.stored().await,
            held,
            "the refused batch moved neither the birthday nor the claim",
        );

        // Equality is the boundary and is legal, because a repeat of a
        // re-anchor that landed presents exactly it.
        self.reanchor(vec![watch.clone()]).await;
        assert_eq!(
            self.stored().await,
            [WatchProgress::unread(watch)],
            "re-anchoring to the birthday already stored settles the claim and \
             is otherwise the same row",
        );
    }
}

impl<R: WatchStore + IndexStore + BalanceStore> WatchContract<'_, R> {
    /// A re-anchor writes the birthday and the claim, and **nothing else**.
    ///
    /// The one property the postcondition wording could not have: a store that
    /// read "hold each observation as given" as "replace the row" would pass
    /// every other re-anchor scenario and silently zero the total the commits
    /// below the new birthday had already earned. Those commits are not undone
    /// by a re-anchor — `unwatch` itself keeps an address's rows — so the total
    /// must not be either.
    pub async fn reanchoring_keeps_what_the_observation_already_counted(&self) {
        let watch = self.watch("wallet", "native", "owner", 1);
        self.add(vec![watch.clone()]).await;
        let anchor = self.commit_anchor().await;
        self.pay(&watch, &anchor).await;
        assert_eq!(
            self.held(&watch).await,
            Decimal::from(5_u64),
            "the fixture block paid the watched address"
        );

        self.reanchor(vec![Watch {
            start: BlockPosition(20),
            ..watch.clone()
        }])
        .await;

        assert_eq!(
            self.held(&watch).await,
            Decimal::from(5_u64),
            "the birthday moved and the balance the rows below it prove did not",
        );
    }

    /// Commits one block above `anchor` that mints 5 to the watched address.
    async fn pay(&self, watch: &Watch, anchor: &BlockRef) {
        let block = BlockRef {
            position: BlockPosition(2),
            height: BlockHeight(2),
            hash: BlockHash(vec![2]),
            parent: Some(BlockParent {
                position: anchor.position,
                hash: anchor.hash.clone(),
            }),
            timestamp: None,
        };
        let facts = CanonicalBlock::new(
            watch.address.network.clone(),
            InterpretedBlock {
                block,
                transactions: vec![ObservationDraft {
                    transaction_id: TransactionRef::new(watch.address.network.clone(), "payment"),
                    transaction_order: 0,
                    status: ObservationDraftStatus::Included,
                    movements: vec![ValueMovement::mint(
                        MovementId("payment-mint".into()),
                        watch.asset.clone(),
                        Decimal::from(5_u64),
                        watch.address.clone(),
                    )],
                    fee: None,
                }],
                outputs: OutputChanges::default(),
                credits: Vec::new(),
            },
        )
        .expect("valid fixture facts");
        let batch = IndexedBatch::new(
            watch.address.network.clone(),
            Some(anchor.clone()),
            vec![facts],
            Vec::new(),
            FeeSettlement::Charged,
        )
        .expect("a connected fixture block");
        self.repository
            .commit(batch)
            .await
            .expect("the fixture block");
    }

    /// The balance the store holds for `watch`'s holding.
    async fn held(&self, watch: &Watch) -> Decimal {
        self.repository
            .balance(BalanceQuery {
                address: watch.address.clone(),
                asset: watch.asset.clone(),
            })
            .await
            .expect("the watched holding")
            .amount
    }
}
