//! The spendable set of one network: the repository's unspent rows, folded over
//! the blocks its index still holds above the confirmed line.
//!
//! The third read of [`PendingJoin`], in a file of its own because its subject
//! is its own. A history page and a balance carry a claim about how much of an
//! address's history is indexed and let the caller decide what a partial answer
//! is worth. This read cannot: its consumer is coin selection, an output it
//! offers becomes a transaction input, and what it adds to a store's page — the
//! pending fold, and the refusal of a cursor folded over different pending
//! blocks — is about which outputs exist right now.
//!
//! The order of the two reads is the one [`super::join`] states, and it
//! matters most here: reading the repository first loses a block committed in
//! between, so the outputs it spent are offered as spendable again.

use std::collections::BTreeSet;

use crate::{
    Address, BlockWithOutputs, IndexError, IndexErrorKind, IndexedOutput, OutputCursor, OutputKey,
    OutputPage, OutputRequest, OutputStore,
};
use base::BoxFuture;

use crate::PendingJoin;

/// The repository's own unspent rows, minus what the pending blocks have
/// already spent, plus what they have created.
///
/// **The one read whose pages must agree with each other.** Outputs the pending
/// blocks spent are removed, and outputs they created are appended to the last
/// page, so paging stays stable while a wallet can still spend its own change
/// at depth one. Those two halves come from one sample, which is what makes a
/// *single* page consistent — and says nothing about the next one.
///
/// It has to, because the consumer is coin selection. It pages until the set is
/// exhausted and spends the union, so a pending block admitted between two
/// pages could hand a caller an input it must not spend *and* the change that
/// exists only because that input was spent: the spend is applied to the page
/// it lands on, and an output already returned by an earlier page is out of
/// reach of `retain`. The durable checkpoint cannot catch it — a block held
/// above the line does not move the checkpoint — so the cursor carries the
/// sample's revision and a later page is folded over the same blocks or
/// refused.
///
/// Refusing, rather than pinning the sample and serving it for the rest of the
/// read, for two reasons. The sample is not reconstructible: a reorg discards
/// held blocks and a re-inspection rewrites them, so by page two the blocks
/// page one folded may not exist anywhere. Keeping a copy per outstanding
/// cursor would be shared mutable state with a lifetime and an eviction rule,
/// and it would answer with a spendable set a reorg has already discarded. A
/// refusal hands back the newest truth instead, and costs a caller nothing new:
/// it is the same retryable `Conflict` the store already raises when the
/// checkpoint moves under a paginated read, which every caller of this port
/// already restarts on.
///
/// Rows that are unspent only as far as the index has read are the
/// repository's to refuse, from the snapshot it answers from, and its refusal
/// passes through here untouched.
impl<R: OutputStore> OutputStore for PendingJoin<R> {
    fn list<'a>(&'a self, request: OutputRequest) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
        Box::pin(async move {
            let address = request.address.clone();
            let sample = self.pending.sampled()?;
            let revision = sample.revision();
            Self::resumable(request.after.as_ref(), revision)?;
            let mut page = self.repository.list(request).await?;
            let pending = sample.above(&page);
            let changes = LiveChanges::fold(&address, &pending);
            page.outputs
                .retain(|output| !changes.spent.contains(&output.key()));
            if page.next.is_none() {
                page.outputs.extend(changes.created);
            }
            // Every cursor this read hands out names the sample it was folded
            // over, which is the only thing the next page has to be checked
            // against: the store's own position and checkpoint it already
            // carries.
            page.next = page.next.map(|next| OutputCursor {
                revision: Some(revision),
                ..next
            });
            Ok(page)
        })
    }
}

impl<R> PendingJoin<R> {
    /// Refuses a later page that would be folded over a different pending set
    /// than the page before it.
    ///
    /// The cursor states which sample it was folded over
    /// ([`OutputCursor::revision`]), so this is that value against the one now
    /// held: equal, and the rest of the read continues over the same blocks;
    /// different, and it is the retryable `Conflict` the caller already restarts
    /// on when the checkpoint moves under a paginated read.
    ///
    /// A cursor carrying no revision at all was not issued here. It is refused
    /// terminally rather than retried, because retrying cannot make a foreign
    /// cursor belong, and serving it would page the store over pending blocks
    /// of this read's own choosing.
    fn resumable(after: Option<&OutputCursor>, revision: u64) -> Result<(), IndexError> {
        let Some(cursor) = after else {
            return Ok(());
        };
        let Some(folded) = cursor.revision else {
            return Err(IndexError::new(
                IndexErrorKind::Store,
                "an output cursor does not belong to this index",
                false,
            ));
        };
        if folded == revision {
            return Ok(());
        }
        Err(IndexError::new(
            IndexErrorKind::Conflict,
            "pending blocks changed during pagination",
            true,
        ))
    }
}

/// What the blocks above the line change about one address's live outputs.
///
/// Selection folds this over the confirmed table rather than filtering that
/// table by depth. A read-time depth filter would be actively wrong: a spend
/// of an old output is already applied while the change it created would be
/// hidden, and Bitcoin's outbox infers "spent" from absence, so the wallet
/// would free the input, see no change, and report a hole. Both halves move
/// together here because they come from the same blocks.
struct LiveChanges {
    /// Outputs those blocks created for the address and no later one spent.
    created: Vec<IndexedOutput>,
    /// Keys those blocks spent, whether or not the store holds them.
    spent: BTreeSet<OutputKey>,
}

impl LiveChanges {
    /// The net effect of `blocks` on `address`'s live outputs.
    fn fold(address: &Address, blocks: &[BlockWithOutputs]) -> Self {
        let mut created = Vec::new();
        let mut spent = BTreeSet::new();
        for changes in blocks.iter().map(|facts| &facts.outputs) {
            for key in changes.spent.iter().chain(&changes.tracked_spends) {
                spent.insert(key.clone());
            }
            created.retain(|output: &IndexedOutput| !spent.contains(&output.key()));
            created.extend(
                changes
                    .created
                    .iter()
                    .filter(|output| output.address == *address)
                    .cloned(),
            );
        }
        Self { created, spent }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base::BlockRef;

    use super::*;
    use crate::storage::join::tests::{
        Held, Landing, address, block, funding, interleaved, network, output, spending,
    };

    /// The whole point of folding the pending blocks: an output the pending
    /// block spends is never offered as spendable, and the output it creates is
    /// offered once, wherever the commit landed around the store read.
    #[test]
    fn a_commit_landing_inside_a_read_neither_resurrects_a_spend_nor_duplicates_a_change() {
        for landing in Landing::ALL {
            let join = interleaved(landing);
            let page = futures_executor::block_on(join.list(OutputRequest {
                network: network(),
                address: address("owner"),
                after: None,
                limit: 10,
            }))
            .expect("live outputs");

            assert_eq!(
                listed(&page),
                ["spend"],
                "{landing:?}: the spent output is gone and the change is listed once"
            );
        }
    }

    /// A store that pages its live outputs the way the sqlite one does: the
    /// cursor names how far into the list the last page reached, and the
    /// checkpoint never moves, so nothing but the pending blocks can change
    /// under a paginated read.
    #[derive(Clone)]
    struct Paged {
        checkpoint: BlockRef,
        outputs: Vec<IndexedOutput>,
    }

    impl OutputStore for Paged {
        fn list<'a>(
            &'a self,
            request: OutputRequest,
        ) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
            let from = request
                .after
                .as_ref()
                .and_then(|cursor| cursor.position.first())
                .map_or(0, |position| usize::from(*position));
            let from = from.min(self.outputs.len());
            let until = from.saturating_add(request.limit).min(self.outputs.len());
            let page = OutputPage {
                checkpoint: Some(self.checkpoint.clone()),
                outputs: self.outputs[from..until].to_vec(),
                next: (until < self.outputs.len()).then(|| OutputCursor {
                    checkpoint: Some(self.checkpoint.clone()),
                    position: vec![until as u8],
                    revision: None,
                }),
            };
            Box::pin(async move { Ok(page) })
        }
    }

    /// The join over two stored outputs, one of which `spending()` spends, as a
    /// consumer holds one: built once, and paged through twice.
    fn paged(held: &Arc<Held>) -> PendingJoin<Paged> {
        PendingJoin::new(
            Paged {
                checkpoint: block(3),
                outputs: vec![funding(), output("second", 2, 50)],
            },
            held.clone(),
        )
    }

    fn one(after: Option<OutputCursor>) -> OutputRequest {
        OutputRequest {
            network: network(),
            address: address("owner"),
            after,
            limit: 1,
        }
    }

    fn listed(page: &OutputPage) -> Vec<String> {
        page.outputs
            .iter()
            .map(|output| output.id.transaction.id.to_string())
            .collect()
    }

    /// Paging the spendable set is *one* read, and a pending block that lands
    /// halfway through it must not be folded into the second half alone.
    ///
    /// The first page hands out the funding output. A pending block then spends
    /// exactly that output and creates change. `retain` can only remove an
    /// output from the page in front of it, so without the revision the cursor
    /// names, the second page appends the change and the caller holds the input
    /// *and* the change that exists only because that input was spent — the one
    /// transaction coin selection must never assemble. Nothing else on the path
    /// can catch it: the durable checkpoint is the same on both pages, which is
    /// what the store and coin selection both validate.
    #[test]
    fn a_pending_block_admitted_between_two_output_pages_refuses_the_rest_of_the_read() {
        let held = Arc::new(Held::default());
        let join = paged(&held);

        let first = futures_executor::block_on(join.list(one(None))).expect("the first page");
        assert_eq!(listed(&first), ["funding"]);
        let cursor = first.next.expect("a second page");

        held.admit(spending());

        let error = futures_executor::block_on(join.list(one(Some(cursor))))
            .expect_err("the rest of the read is refused");
        assert_eq!(error.kind, IndexErrorKind::Conflict);
        assert_eq!(error.message, "pending blocks changed during pagination");
        assert!(
            error.retryable,
            "the caller restarts the read, as it already does when the checkpoint moves"
        );
    }

    /// And pending blocks that do not move page to the end as before: the
    /// revision the cursor names survives the round trip through the store, the
    /// spend applies on the page that holds it, and the change is appended once.
    #[test]
    fn unchanged_pending_blocks_page_to_the_end_and_append_their_change_once() {
        let held = Arc::new(Held::default());
        held.admit(spending());
        let join = paged(&held);

        let first = futures_executor::block_on(join.list(one(None))).expect("the first page");
        assert!(
            listed(&first).is_empty(),
            "the pending block spent the only output on this page"
        );
        let cursor = first.next.expect("a second page");
        let second = futures_executor::block_on(join.list(one(Some(cursor))))
            .expect("the last page over the same pending blocks");

        assert_eq!(listed(&second), ["second", "spend"]);
        assert_eq!(second.next, None);
    }

    /// The cursor handed out is the store's own with one field added: the
    /// backend position is byte-for-byte what the store issued, and the sample
    /// the page was folded over is *named* rather than packed into those bytes.
    /// Which is the whole of what the caller carries back.
    #[test]
    fn an_issued_output_cursor_keeps_the_store_position_and_names_the_sample() {
        let held = Arc::new(Held::default());
        held.admit(spending());
        let join = paged(&held);

        let cursor = futures_executor::block_on(join.list(one(None)))
            .expect("the first page")
            .next
            .expect("a second page");

        assert_eq!(cursor.position, vec![1], "the store's own resume position");
        assert_eq!(cursor.checkpoint, Some(block(3)));
        assert_eq!(cursor.revision, Some(held.revision()));
    }

    /// A cursor that was not issued here names no sample to check against, and
    /// a read that trusted it would page the store over pending blocks of its
    /// own choosing. It is the store's own refusal, for the same reason.
    #[test]
    fn an_output_cursor_issued_elsewhere_is_refused() {
        let join = paged(&Arc::new(Held::default()));
        let foreign = OutputCursor {
            checkpoint: Some(block(3)),
            position: vec![1],
            revision: None,
        };

        let error = futures_executor::block_on(join.list(one(Some(foreign))))
            .expect_err("a cursor from another store");

        assert_eq!(error.kind, IndexErrorKind::Store);
        assert_eq!(
            error.message,
            "an output cursor does not belong to this index"
        );
        assert!(!error.retryable);
    }

    /// A refusal the repository makes — rows it cannot prove unspent, because
    /// the address is still being inspected below the line — reaches the caller
    /// as the repository made it. Nothing here retries, weakens or replaces it.
    #[test]
    fn a_refusal_the_repository_makes_reaches_the_caller_unchanged() {
        struct Refusing;

        impl OutputStore for Refusing {
            fn list<'a>(
                &'a self,
                _request: OutputRequest,
            ) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
                Box::pin(async {
                    Err(IndexError::new(
                        IndexErrorKind::Conflict,
                        "not yet proven unspent",
                        true,
                    ))
                })
            }
        }

        let held = Arc::new(Held::default());
        held.admit(spending());
        let error = futures_executor::block_on(PendingJoin::new(Refusing, held).list(one(None)))
            .expect_err("the repository's refusal");

        assert_eq!(error.kind, IndexErrorKind::Conflict);
        assert_eq!(error.message, "not yet proven unspent");
        assert!(error.retryable);
    }
}
