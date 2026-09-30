//! One network's pending-inclusive reads: a repository's own answers, completed
//! by the blocks its index still holds above the confirmed line.
//!
//! Neither half can answer alone. A repository holds confirmed rows and answers
//! at the checkpoint its answer carries; an index holds the blocks above that
//! line and answers no query at all, only a sample of them
//! ([`indexing::Pending`]). Composing the two is the consumer's job, and
//! [`PendingJoin`] is that composition: [`crate::History`], [`crate::Balances`]
//! and the spendable set, all three over one value so they cannot disagree
//! about what is confirmed and what is on its way.
//!
//! **A commit is two moments, and a read can land between them.** The store
//! takes a block, and then the index drops it from the blocks it holds. Every
//! read here is therefore made in one order, and the order is the guarantee:
//!
//! 1. sample the pending blocks;
//! 2. read the repository;
//! 3. narrow the sample by **that answer**, which carries the checkpoint it
//!    was read at ([`indexing::PendingSample::above`]).
//!
//! Reading the repository first loses a block committed in between: neither
//! half holds it. Narrowing by a checkpoint fetched beside the answer counts it
//! twice, or not at all, because that line is not the one the rows were read
//! at. The `Interleaved` fixture below lands a commit around each read, at
//! every place one can land, and each read's own tests hold it to the same
//! answer.
//!
//! **What is not decided here.** How much of an address's history the rows
//! cover is the repository's own claim, made from the snapshot the rows came
//! from, and it is passed on unchanged: the blocks above the line say nothing
//! about the range below it.

use std::sync::Arc;

use indexing::Pending;

/// One network's repository joined to the pending blocks of the index that
/// fills it.
///
/// It answers [`crate::History`] over a repository that pages its own history,
/// [`crate::Balances`] over one that folds a balance, and
/// [`indexing::OutputStore`] over one that lists live outputs — each of them the
/// repository's answer with the same sample of pending blocks folded in.
///
/// **Build one per network, from the pair that belongs together.** A chain's
/// composition holds both the repository it handed its index and the index
/// itself, so it is the place that can pair them; a consumer serving several
/// networks holds one of these per network ([`crate::Composition`]).
///
/// The index is held as the one capability read here, never as the indexer:
/// nothing in a read fetches, interprets or commits a block.
pub struct PendingJoin<R> {
    pub(crate) repository: R,
    pub(crate) pending: Arc<dyn Pending>,
}

impl<R> PendingJoin<R> {
    /// The reads of `repository`, completed by what `pending` holds above the
    /// line `repository` reports.
    ///
    /// `pending` must be the index that commits into `repository`, for the same
    /// network: a sample of another network's blocks would be folded over rows
    /// they were never above.
    #[must_use]
    pub fn new(repository: R, pending: Arc<dyn Pending>) -> Self {
        Self {
            repository,
            pending,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };

    use base::{BlockRef, BoxFuture};
    use indexing::{
        Address, AssetId, Balance, BalanceQuery, BalanceStore, BlockHash, BlockHeight,
        BlockPosition, CanonicalBlock, CanonicalPage, CanonicalStatus, CanonicalTransaction,
        ChainName, Completeness, ConfirmedHistory, Decimal, HistoryQuery, IndexError,
        IndexedOutput, InterpretedBlock, MovementId, Network, ObservationDraft,
        ObservationDraftStatus, OutputChanges, OutputId, OutputPage, OutputRequest, OutputStore,
        Page, PendingValue, Subject, TransactionRef, ValueMovement,
    };

    use super::*;
    use crate::{Balances, History};

    /// The blocks an index holds above its line, as a test holds them: a list
    /// the scenario changes between the two moments of a commit.
    ///
    /// It owes what every [`Pending`] owes — commit order, and a revision that
    /// moves with every change — and nothing else. That an index's own list
    /// keeps the same two properties is the index's test, not this crate's.
    #[derive(Default)]
    pub(crate) struct Held(Mutex<(Vec<CanonicalBlock>, u64)>);

    impl Held {
        /// Holds `block` above the line.
        pub(crate) fn admit(&self, block: CanonicalBlock) {
            let mut held = self.0.lock().expect("the held blocks");
            held.0.push(block);
            held.1 += 1;
        }

        /// Drops the oldest held block, which storage has just taken.
        pub(crate) fn committed(&self, block: &BlockRef) {
            let mut held = self.0.lock().expect("the held blocks");
            assert_eq!(
                held.0.first().map(CanonicalBlock::block),
                Some(block),
                "only the oldest held block commits"
            );
            held.0.remove(0);
            held.1 += 1;
        }

        /// The revision the next sample will carry.
        pub(crate) fn revision(&self) -> u64 {
            self.0.lock().expect("the held blocks").1
        }
    }

    impl Pending for Held {
        fn sampled(&self) -> Result<indexing::PendingSample, IndexError> {
            let held = self.0.lock().expect("the held blocks");
            Ok(indexing::PendingSample::new(held.0.clone(), held.1))
        }
    }

    pub(crate) fn network() -> Network {
        Network::new(ChainName::new("test"), "mainnet")
    }

    pub(crate) fn block(height: u64) -> BlockRef {
        BlockRef {
            position: BlockPosition(height),
            height: BlockHeight(height),
            hash: BlockHash(vec![height as u8]),
            parent: None,
            timestamp: None,
        }
    }

    pub(crate) fn address(value: &str) -> Address {
        Address::new(network(), value)
    }

    pub(crate) fn owner() -> Subject {
        Subject {
            address: address("owner"),
            asset: AssetId::native(network().chain),
        }
    }

    pub(crate) fn transaction(height: u64) -> CanonicalTransaction {
        CanonicalTransaction {
            transaction_id: TransactionRef::new(network(), "tx"),
            transaction_order: 0,
            status: CanonicalStatus::Included {
                block: block(height),
            },
            movements: Vec::new(),
            fee: None,
        }
    }

    /// A repository that answers one page, whatever it is asked.
    #[derive(Clone)]
    pub(crate) struct Paged(pub(crate) CanonicalPage);

    impl ConfirmedHistory for Paged {
        fn history<'a>(
            &'a self,
            _request: HistoryQuery,
        ) -> BoxFuture<'a, Result<CanonicalPage, IndexError>> {
            Box::pin(async move { Ok(self.0.clone()) })
        }
    }

    pub(crate) fn page(
        checkpoint: Option<BlockRef>,
        transactions: Vec<CanonicalTransaction>,
    ) -> CanonicalPage {
        CanonicalPage {
            checkpoint,
            transactions,
            next: None,
            completeness: Completeness::Incomplete,
        }
    }

    pub(crate) fn query(after: Option<indexing::HistoryPosition>) -> HistoryQuery {
        HistoryQuery {
            address: address("owner"),
            asset: AssetId::native(network().chain),
            after,
            limit: 10,
        }
    }

    /// One transfer of `amount` from `payer` to `to` at `height`, as an index
    /// holds it.
    pub(crate) fn admitted(height: u64, amount: u64, to: &str) -> CanonicalBlock {
        let network = network();
        let draft = ObservationDraft {
            transaction_id: TransactionRef::new(network.clone(), format!("tx-{height}")),
            transaction_order: 0,
            status: ObservationDraftStatus::Included,
            movements: vec![ValueMovement::transfer(
                MovementId("0".into()),
                AssetId::native(network.chain.clone()),
                Decimal::from(amount),
                address("payer"),
                address(to),
            )],
            fee: None,
        };
        let interpreted =
            InterpretedBlock::new(&network, block(height), vec![draft]).expect("one transaction");
        CanonicalBlock::new(network, interpreted).expect("a valid pending block")
    }

    /// Where an interleaved commit lands around a durable read.
    ///
    /// A commit is not one instant even inside the pass: storage takes the
    /// block first, and the index drops it from the blocks it holds afterwards.
    /// A read can land on either side of either moment.
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum Landing {
        /// The store answered from before the commit, and the index dropped
        /// the block while that answer was on its way.
        Dropped,
        /// The store answered from after the commit, while the index still
        /// held the block.
        Held,
        /// The store answered from before the commit and took the block the
        /// moment that answer had left, while the index still held it.
        ///
        /// To a join that reads the store once this is the state before any
        /// commit. It is here for the join that reads it twice: a line read
        /// beside the rows is then a line the store has already moved past, or
        /// has not reached yet, and narrowing by it counts the block on both
        /// sides or on neither.
        Trailing,
    }

    impl Landing {
        /// Every place a commit lands around a read.
        pub(crate) const ALL: [Self; 3] = [Self::Dropped, Self::Held, Self::Trailing];
    }

    /// Everything the store holds at one checkpoint.
    #[derive(Clone)]
    struct Stored {
        checkpoint: BlockRef,
        outputs: Vec<IndexedOutput>,
        transactions: Vec<CanonicalTransaction>,
        amount: u64,
    }

    /// A store that lands the pending block's commit *during* its own read.
    ///
    /// The race written down: no sleep, no second thread, no scheduler. The
    /// commit happens at the one instant a reader cannot observe, and which
    /// half of it the read returns is the fixture's only parameter.
    ///
    /// It reaches the very list the join samples, which is the only state the
    /// race is about: a join that read the store before sampling would sample a
    /// list this read has already dropped the block from.
    ///
    /// **The commit lands once, around the first read.** Every later read
    /// answers from after it, as a store that has taken a block does. So a join
    /// that reads twice, to fetch a line beside the rows, is answered by a store
    /// that moved between its two reads, and what it reports is held to the same
    /// assertion as the join that reads once.
    #[derive(Clone)]
    pub(crate) struct Interleaved {
        landing: Landing,
        held: Arc<Held>,
        /// Whether storage has taken block four. Shared by every clone, because
        /// they are one store.
        taken: Arc<AtomicBool>,
        claim: Completeness,
        before: Stored,
        after: Stored,
    }

    impl Interleaved {
        pub(crate) fn new(landing: Landing, held: Arc<Held>) -> Self {
            Self {
                landing,
                held,
                taken: Arc::default(),
                claim: Completeness::Incomplete,
                before: Stored {
                    checkpoint: block(3),
                    outputs: vec![funding()],
                    transactions: vec![transaction(2)],
                    amount: 100,
                },
                after: Stored {
                    checkpoint: block(4),
                    outputs: vec![change()],
                    transactions: vec![transaction(2), committed()],
                    amount: 110,
                },
            }
        }

        /// The store's answer, with the commit landing around the first one.
        fn read(&self) -> &Stored {
            if self.taken.swap(true, Ordering::SeqCst) {
                return &self.after;
            }
            match self.landing {
                Landing::Dropped => {
                    self.held.committed(&block(4));
                    &self.before
                }
                Landing::Held => &self.after,
                Landing::Trailing => &self.before,
            }
        }
    }

    impl ConfirmedHistory for Interleaved {
        fn history<'a>(
            &'a self,
            _request: HistoryQuery,
        ) -> BoxFuture<'a, Result<CanonicalPage, IndexError>> {
            let stored = self.read();
            let page = CanonicalPage {
                completeness: self.claim,
                ..page(Some(stored.checkpoint.clone()), stored.transactions.clone())
            };
            Box::pin(async move { Ok(page) })
        }
    }

    impl BalanceStore for Interleaved {
        fn balance<'a>(
            &'a self,
            _query: BalanceQuery,
        ) -> BoxFuture<'a, Result<Balance, IndexError>> {
            let stored = self.read();
            let balance = Balance {
                amount: Decimal::from(stored.amount),
                observed_at: Some(stored.checkpoint.clone()),
                pending: PendingValue::none(),
                completeness: self.claim,
            };
            Box::pin(async move { Ok(balance) })
        }
    }

    impl OutputStore for Interleaved {
        fn list<'a>(
            &'a self,
            _request: OutputRequest,
        ) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
            let stored = self.read();
            let page = OutputPage {
                checkpoint: Some(stored.checkpoint.clone()),
                outputs: stored.outputs.clone(),
                next: None,
            };
            Box::pin(async move { Ok(page) })
        }
    }

    pub(crate) fn output(id: &str, height: u64, amount: u64) -> IndexedOutput {
        IndexedOutput {
            id: OutputId {
                transaction: TransactionRef::new(network(), id),
                index: 0,
            },
            address: address("owner"),
            asset: AssetId::native(network().chain),
            amount: Decimal::from(amount),
            evidence: Vec::new(),
            created_at: BlockHeight(height),
            coinbase: false,
        }
    }

    /// The output block four spends.
    pub(crate) fn funding() -> IndexedOutput {
        output("funding", 2, 100)
    }

    /// The output block four creates.
    fn change() -> IndexedOutput {
        output("spend", 4, 110)
    }

    /// Block four's transaction, as the store holds it once committed.
    fn committed() -> CanonicalTransaction {
        CanonicalTransaction {
            transaction_id: TransactionRef::new(network(), "tx-4"),
            ..transaction(4)
        }
    }

    /// Block four, as an index holds it: one credit, one spend, one change
    /// output.
    pub(crate) fn spending() -> CanonicalBlock {
        let draft = ObservationDraft {
            transaction_id: TransactionRef::new(network(), "tx-4"),
            transaction_order: 0,
            status: ObservationDraftStatus::Included,
            movements: vec![ValueMovement::transfer(
                MovementId("0".into()),
                AssetId::native(network().chain),
                Decimal::from(10_u64),
                address("payer"),
                address("owner"),
            )],
            fee: None,
        };
        let interpreted = InterpretedBlock {
            outputs: OutputChanges {
                created: vec![change()],
                spent: vec![funding().key()],
                tracked_spends: Vec::new(),
            },
            ..InterpretedBlock::new(&network(), block(4), vec![draft]).expect("one transaction")
        };
        CanonicalBlock::new(network(), interpreted).expect("a valid pending block")
    }

    /// The join over a store whose read a commit of block four lands inside.
    pub(crate) fn interleaved(landing: Landing) -> PendingJoin<Interleaved> {
        let held = Arc::new(Held::default());
        held.admit(spending());
        PendingJoin::new(Interleaved::new(landing, held.clone()), held)
    }

    /// The claim about the range behind the rows is the repository's, and both
    /// reads pass it on exactly as it was made.
    ///
    /// Nothing here has the facts to change it: how far an address has been
    /// inspected is a row of the store, read in the snapshot the answer came
    /// from, and the blocks above the line say nothing about the range below
    /// it. A join that weakened the claim would be repairing an answer it cannot
    /// check, and one that strengthened it would be inventing one.
    #[test]
    fn both_reads_pass_on_the_claim_the_repository_made() {
        for claim in [
            Completeness::CompleteHistory,
            Completeness::Incomplete,
            Completeness::Overdrawn,
        ] {
            let held = Arc::new(Held::default());
            held.admit(admitted(4, 5, "owner"));
            let stored = CanonicalPage {
                completeness: claim,
                ..page(Some(block(3)), vec![transaction(2)])
            };
            let history = PendingJoin::new(Paged(stored), held.clone());
            let page = futures_executor::block_on(history.history(&owner(), Page::first(10)))
                .expect("a history page");
            assert_eq!(page.completeness, claim);

            let folds = PendingJoin::new(
                Interleaved {
                    claim,
                    ..Interleaved::new(Landing::Held, held)
                },
                Arc::new(Held::default()),
            );
            let balance = futures_executor::block_on(folds.balance(&owner())).expect("a balance");
            assert_eq!(balance.completeness, claim);
        }
    }
}
