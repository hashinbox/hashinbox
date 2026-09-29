//! One network's read side: the repository's rows, and the blocks the index
//! over it still holds above the confirmed line.
//!
//! Both halves answer from one owner, so history, balance and UTXO selection
//! cannot disagree about what is confirmed and what is on its way — and the
//! blocks a read folds in are the very ones the next pass will commit, because
//! there is only one collection of them.
//!
//! **A read is a value, and it names what it reads.** [`ObservedBalance`],
//! [`ObservedHistory`] and [`ObservedOutputs`] each hold three things — the
//! repository, the blocks above that repository's confirmed line, and how far
//! each observation of an address has been inspected — and nothing else: not
//! the block source, not the interpreter, not the [`Service`] that advances
//! them. Querying belongs to the repository; what these add is the half a
//! repository cannot know, which is why none of them is an `impl` on the thing
//! doing the indexing. [`Service`] keeps the *constructors*
//! ([`Service::balances`], [`Service::history`], [`Service::outputs`]) because
//! it is the one value holding all three at once, and handing them over
//! together is what stops a reader pairing one network's rows with another's
//! overlay. All three are the same shape; [`ObservedBalance`] says what the
//! last `impl` on [`Service`] cost while it was one.
//!
//! **[`ObservedOutputs`] lives beside this file, not in it.** It is the one
//! read whose refusals are about coin selection rather than about how much of
//! an address's history is indexed, and it says so in its own module doc. The
//! `#[path]` declaration keeps it a submodule of this one — `pub(super)` still
//! means visible here, which is what [`Service::outputs`] builds it through —
//! without a directory holding a single file (`scripts/check-chains.sh`
//! check 11).

#[path = "read_outputs.rs"]
mod outputs;

use std::sync::Arc;

pub use outputs::ObservedOutputs;

use crate::{
    Balance, BalanceQuery, BalanceStore, Balances, BlockInterpreter, BlockRef, BoxFuture,
    CanonicalBlock, CanonicalPage, CanonicalStatus, CanonicalTransaction, Checkpoint,
    ConfirmedHistory, History, HistoryQuery, Holder, IndexError, IndexErrorKind, IndexStore,
    Network, ObservedTransaction, Page, PendingValue, Service, TransactionPage, TransactionStatus,
    overlay::Frontier, watches::Watches,
};

impl<S, I, R> Checkpoint for Service<S, I, R>
where
    S: Send + Sync,
    I: BlockInterpreter,
    R: IndexStore,
{
    fn checkpoint<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        self.repository.checkpoint(network)
    }
}

/// The three readers over this service's own network, each handed the three
/// things it reads and no more.
///
/// A constructor rather than a capability: what a query answers is the
/// repository's, and this type's part in it is that it is the one value holding
/// the rows' owner, the blocks above that owner's confirmed line, and the
/// inspection progress a read is gated on — so it is the only place the three
/// can be paired without a caller being able to pair the wrong ones. `R` is
/// cloned into the reader, which is the same handle every network already
/// shares; nothing here is a borrow of the service.
impl<S, I, R> Service<S, I, R>
where
    I: BlockInterpreter,
    R: Clone,
{
    /// This network's history reader.
    #[must_use]
    pub fn history(&self) -> ObservedHistory<R> {
        ObservedHistory {
            repository: self.repository.clone(),
            pending: Arc::clone(&self.pending),
            watches: Arc::clone(&self.watches),
        }
    }

    /// This network's balance reader.
    #[must_use]
    pub fn balances(&self) -> ObservedBalance<R> {
        ObservedBalance {
            repository: self.repository.clone(),
            pending: Arc::clone(&self.pending),
            watches: Arc::clone(&self.watches),
        }
    }

    /// This network's spendable-output reader.
    #[must_use]
    pub fn outputs(&self) -> ObservedOutputs<R> {
        ObservedOutputs {
            repository: self.repository.clone(),
            pending: Arc::clone(&self.pending),
            watches: Arc::clone(&self.watches),
        }
    }
}

/// One network's history, confirmed and pending: a repository's own confirmed
/// pages, narrowed and completed by the blocks its index still holds above the
/// line.
///
/// **The third thing the split needs.** A repository holds confirmed facts and
/// answers [`ConfirmedHistory`] over them; an index holds the blocks above its
/// own confirmed line, and nothing else knows them. Neither half alone can
/// answer what `apps/api` serves — confirmed *plus* pending — so the join is a
/// value, and it sits here beside the balance and output reads because it folds
/// the very same overlay sample in the very same order.
///
/// **Optional, and in no contract.** No sink implements it, no
/// [`crate::Repository`] names it, no index carries it, and [`crate::Index`]
/// does not route to it. A consumer serving several networks holds one reader
/// per network and asks the one that owns the subject.
///
/// **It names its dependencies.** The three fields are the whole of what a page
/// is folded from, so the reader is generic over the repository alone: a block
/// source and a block interpreter are how the overlay got filled, not something
/// a read consults, and while this value held a [`Service`] they were type
/// parameters of it anyway. [`Service::history`] is how one is built, because
/// only the service holds all three together.
pub struct ObservedHistory<R> {
    repository: R,
    pending: Arc<Frontier>,
    watches: Arc<Watches>,
}

/// The same two gates the balance read takes, on the same two facts, over the
/// claim the same store made about the same rows — and one narrowing the
/// balance read must not take.
///
/// A page is a read of stored rows, and a backfill exists precisely because
/// rows are missing below the ones it returns. `next` cannot carry that: it
/// speaks about what follows the page, and a backfill inserts what precedes
/// it. So the page's claim goes through
/// [`while_backfilling`](crate::Completeness::while_backfilling) whenever one
/// is outstanding for this address, exactly as
/// [`Balances::balance`](crate::Balances::balance) does, and for the reason
/// `docs/design/030 §8 (ex-032)` states: history pagination during catch-up must not
/// promise a complete export while older rows are still being inserted. Then
/// through [`without_a_birthday_proof`](crate::Completeness::without_a_birthday_proof),
/// the axis a store cannot be trusted on, for the same reason a fold is.
///
/// **Both are checks on an answer, not the answer.** The claim itself is
/// [`CanonicalPage::completeness`], which the store reads off the observation's
/// own row inside the snapshot the page came from. Assembling it here instead
/// made both gates unfalsifiable — a constant that each gate maps to itself is
/// a value no gate can be observed to have touched — and deleting either of
/// them left the whole suite green.
///
/// **And the third narrowing, which is not a gate at all.** The two above ask
/// the watch set a question about this address; this one asks nothing, because
/// what it settles is the shape of the answer.
/// [`Completeness::VerifiedOpening`](crate::Completeness::VerifiedOpening) is
/// an independent node read agreeing with the fold about a *total* at one
/// block, and a page reports no total, so the value is meaningless on one. One
/// vocabulary stays on the wire — `apps/api` maps the same enum for both reads
/// and `openapi_contract` holds it to that — and the page path narrows at the
/// boundary instead
/// ([`without_a_reported_amount`](crate::Completeness::without_a_reported_amount)).
/// [`Balances::balance`] must not take it: there the agreement is about the very
/// number being returned, and it is the only positive proof any chain produces.
impl<R: ConfirmedHistory> History for ObservedHistory<R> {
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
            let line = page.checkpoint.clone();
            let pending = sample.above(line.as_ref());
            let mut page = self.observe(&request, page, &pending)?;
            // The same checkpoint the rows came with, which is why the store owes
            // one consistent read: a gate taken against a different line could
            // call a page complete that the line it was read at says is short.
            if self.watches.backfilling(of.address(), line.as_ref())? {
                page.completeness = page.completeness.while_backfilling();
            }
            if !self.watches.observed_from_the_first_block(of.address())? {
                page.completeness = page.completeness.without_a_birthday_proof();
            }
            page.completeness = page.completeness.without_a_reported_amount();
            Ok(page)
        })
    }
}

/// One network's balance: the store's own fold over its durable rows,
/// completed by the blocks this index still holds above the line.
///
/// The same three dependencies [`ObservedHistory`] names, for the same reason,
/// and built the same way ([`Service::balances`]). Nothing here can produce
/// display units, because nothing here knows an asset's decimals.
///
/// **Not an `impl` on [`Service`], and that was the last one.** A fold is a
/// read of a store's rows, so answering it was never the indexer's to do; while
/// it was written there, `Service` was a [`Balances`] implementation that no
/// consumer ever reached that way — every one of them holds `Arc<dyn Balances>`
/// per network (`wallets::Composition::balance`) — and the chain configs coerced
/// the whole indexer into that handle to hand it over.
pub struct ObservedBalance<R> {
    repository: R,
    pending: Arc<Frontier>,
    watches: Arc<Watches>,
}

/// Where a fold stops claiming more than the index holds. The store folds
/// whatever rows exist; only the index knows some of this address's rows are
/// not written yet, because it is the thing still writing them. So the store's
/// `Completeness` goes through
/// [`while_backfilling`](crate::Completeness::while_backfilling) whenever one
/// is outstanding — one gate on the read every caller takes, rather than a
/// second notion of incompleteness beside the one the type already has.
impl<R: BalanceStore> Balances for ObservedBalance<R> {
    fn balance<'a>(&'a self, of: &'a dyn Holder) -> BoxFuture<'a, Result<Balance, IndexError>> {
        Box::pin(async move {
            let query = BalanceQuery::new(of);
            query.validate()?;
            let sample = self.pending.sampled()?;
            let mut balance = self.repository.balance(query).await?;
            let line = balance.observed_at.clone();
            let pending = sample.above(line.as_ref());
            balance.pending = PendingValue::fold(of, &pending);
            if self.watches.backfilling(of.address(), line.as_ref())? {
                balance.completeness = balance.completeness.while_backfilling();
            }
            // The other axis, and the only one a store cannot be trusted on: a
            // claim to hold an address's *whole* history is refused for an
            // observation that began above the chain's first block, because the
            // range below a birthday is one no backfill will ever read.
            if !self.watches.observed_from_the_first_block(of.address())? {
                balance.completeness = balance.completeness.without_a_birthday_proof();
            }
            Ok(balance)
        })
    }
}

/// The read-time half of a page: which block confirmations count from, which
/// uncommitted transactions belong on it, and what each row's status is.
///
/// All of it is the reader's, not the store's. A repository answers canonical
/// rows at a checkpoint; a confirmation count and a `Pending` status are
/// statements about the line those rows were read at, which is why they are
/// derived here and nowhere in [`Service`].
impl<R> ObservedHistory<R> {
    /// Confirmations count from the newest block anything is known about, so
    /// the number keeps meaning "how deep is this" across the line.
    fn depth_from(page: &CanonicalPage, pending: &[CanonicalBlock]) -> Option<BlockRef> {
        pending
            .last()
            .map(|facts| facts.block().clone())
            .or_else(|| page.checkpoint.clone())
    }

    fn observe(
        &self,
        request: &HistoryQuery,
        page: CanonicalPage,
        pending: &[CanonicalBlock],
    ) -> Result<TransactionPage, IndexError> {
        let tip = Self::depth_from(&page, pending);
        let completeness = page.completeness;
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
            // What the store's own rows can claim, before the index weakens it
            // with what it knows is still being written: the very answer a fold
            // of the same rows carries (`crate::Balance`), read from the same
            // observation row inside the same snapshot as the page.
            completeness,
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

    /// `Confirmed` is no longer a threshold but a fact: a transaction is
    /// confirmed when the durable store holds it, which is when it is at or
    /// below the chain's confirmed line and will not revert.
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

    use base::BlockHeight;

    use super::*;
    use crate::{
        Address, AssetId, BlockHash, BlockPosition, ChainName, Decimal, FeeSettlement,
        IndexedBlock, InterpretedBlock, MovementId, OutputChanges, OutputCursor, OutputPage,
        OutputRequest, OutputStore, SyncConfig, SyncPolicy, TransactionRef, ValueMovement,
        overlay::Frontier,
    };

    /// The chain a read never reaches.
    ///
    /// Every scenario here is a read, and a read touches neither the source nor
    /// `inspect`: it samples the overlay, asks the store, and folds. The one
    /// thing it does ask the interpreter is where this chain settles its fee,
    /// which is the whole reason the fold needs one at all.
    struct Chain;

    /// A native block that is only ever its own reference.
    #[derive(Clone)]
    struct Native(BlockRef);

    impl IndexedBlock for Native {
        fn block_ref(&self) -> BlockRef {
            self.0.clone()
        }
    }

    impl crate::BlockInterpreter for Chain {
        type Block = Native;

        fn inspect(
            &self,
            _block: &Self::Block,
            _addresses: &[Address],
        ) -> Result<InterpretedBlock, IndexError> {
            unreachable!("a read never interprets a block")
        }

        fn settlement(&self) -> FeeSettlement {
            FeeSettlement::Charged
        }
    }

    /// One service over `repository`, holding `overlay` above the confirmed
    /// line. The source is `()`, because a read has nothing to ask it.
    fn service<R>(repository: R, overlay: Arc<Frontier>) -> Service<(), Chain, R> {
        Service::over(
            (),
            Chain,
            repository,
            SyncConfig::new(&network(), SyncPolicy::new(10)).expect("a valid policy"),
            overlay,
        )
    }

    #[derive(Clone)]
    struct Repository(CanonicalPage);

    impl ConfirmedHistory for Repository {
        fn history<'a>(
            &'a self,
            _request: HistoryQuery,
        ) -> BoxFuture<'a, Result<CanonicalPage, IndexError>> {
            Box::pin(async move { Ok(self.0.clone()) })
        }
    }

    /// A watch list that forgets everything: the hot set is what these
    /// scenarios read, and a store that restored rows would decide the answer
    /// instead of the watch the scenario adds.
    impl crate::WatchStore for Repository {
        fn add<'a>(&'a self, _watches: Vec<crate::Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
            Box::pin(async { Ok(()) })
        }

        fn all<'a>(
            &'a self,
            _network: &'a Network,
        ) -> BoxFuture<'a, Result<Vec<crate::WatchProgress>, IndexError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn forget<'a>(
            &'a self,
            _network: &'a Network,
            _watch: &'a crate::Watch,
        ) -> BoxFuture<'a, Result<(), IndexError>> {
            Box::pin(async { Ok(()) })
        }

        fn reanchor<'a>(
            &'a self,
            _watches: Vec<crate::Watch>,
        ) -> BoxFuture<'a, Result<(), IndexError>> {
            Box::pin(async { Ok(()) })
        }
    }

    fn network() -> Network {
        Network {
            chain: ChainName::new("test"),
            name: "mainnet".into(),
        }
    }

    fn block(height: u64) -> BlockRef {
        BlockRef {
            position: BlockPosition(height),
            height: BlockHeight(height),
            hash: BlockHash(vec![height as u8]),
            parent: None,
            timestamp: None,
        }
    }

    fn address(value: &str) -> Address {
        Address::new(network(), value)
    }

    fn transaction(network: &Network, height: u64) -> CanonicalTransaction {
        CanonicalTransaction {
            transaction_id: TransactionRef {
                network: network.clone(),
                id: "tx".into(),
            },
            transaction_order: 0,
            status: CanonicalStatus::Included {
                block: block(height),
            },
            movements: Vec::new(),
            fee: None,
        }
    }

    fn index(page: CanonicalPage) -> Arc<Service<(), Chain, Repository>> {
        Arc::new(service(Repository(page), Arc::new(Frontier::default())))
    }

    /// What answers a history page now that the join is a value of its own and
    /// not one more capability of the service.
    fn reader<R: Clone>(index: &Arc<Service<(), Chain, R>>) -> ObservedHistory<R> {
        index.history()
    }

    fn query(after: Option<crate::HistoryPosition>) -> HistoryQuery {
        HistoryQuery {
            address: address("owner"),
            asset: AssetId::native(network().chain),
            after,
            limit: 10,
        }
    }

    /// One credit of `amount` to `owner` at `height`, as the overlay holds it.
    fn admitted(height: u64, amount: u64, to: &str) -> CanonicalBlock {
        let network = network();
        let draft = crate::ObservationDraft {
            transaction_id: TransactionRef::new(network.clone(), format!("tx-{height}")),
            transaction_order: 0,
            status: crate::ObservationDraftStatus::Included,
            movements: vec![ValueMovement::transfer(
                MovementId("0".into()),
                AssetId::native(network.chain.clone()),
                Decimal::from(amount),
                address("payer"),
                address(to),
            )],
            fee: None,
        };
        CanonicalBlock::new(
            network,
            InterpretedBlock {
                block: block(height),
                transactions: vec![draft],
                outputs: OutputChanges::default(),
                credits: Vec::new(),
            },
        )
        .expect("a valid pending block")
    }

    /// A store that makes `claim` on every page, so each narrowing below has
    /// something to refuse.
    ///
    /// Without it none of them could be observed at all: a page's claim used to
    /// be assembled here as the constant `Incomplete`, and a constant that every
    /// narrowing maps to itself is a value none of them can be seen to have
    /// touched. The scenarios below were then true by construction, and deleting
    /// either gate left the suite green.
    ///
    /// The claim is a parameter because the three narrowings refuse different
    /// ones: the two gates weaken a *range* claim, and the third refuses
    /// evidence about a *total* that no range claim carries.
    fn claiming(
        claim: crate::Completeness,
        page: CanonicalPage,
    ) -> Arc<Service<(), Chain, Repository>> {
        index(CanonicalPage {
            completeness: claim,
            ..page
        })
    }

    /// One watched subject and the page read for it.
    fn read_for(
        index: &Arc<Service<(), Chain, Repository>>,
        start: BlockPosition,
    ) -> TransactionPage {
        let of = crate::Subject {
            address: address("owner"),
            asset: AssetId::native(network().chain),
        };
        futures_executor::block_on(index.watches.add(
            &index.repository,
            "watched",
            &of,
            crate::Start::At(start),
            Some(block(3)),
        ))
        .expect("the observation");
        futures_executor::block_on(reader(index).history(&of, crate::Page::first(10)))
            .expect("a history page")
    }

    /// A page is a read of the rows a backfill is still inserting *below* it,
    /// and `next` cannot say so: it speaks about what follows the page, not
    /// what precedes it. So the page carries the same claim a fold does and
    /// takes the same gate, which is what stops a caller who paged to the end
    /// mid-backfill from holding what reads as a complete export
    /// (`docs/design/030 §8 (ex-032)`, consumer reads).
    ///
    /// The observation begins at the chain's first block, so the *other* gate
    /// is a no-op here and this one is the only thing that can weaken the
    /// store's claim. Deleting it hands a caller `CompleteHistory` over rows
    /// that are still arriving.
    #[test]
    fn a_history_page_never_claims_completeness_while_a_backfill_is_outstanding() {
        let index = claiming(
            crate::Completeness::CompleteHistory,
            CanonicalPage {
                checkpoint: Some(block(3)),
                transactions: vec![transaction(&network(), 2)],
                next: None,
                completeness: crate::Completeness::Incomplete,
            },
        );
        let owner = address("owner");
        let page = read_for(&index, BlockPosition(0));
        assert!(
            index
                .watches
                .backfilling(&owner, Some(&block(3)))
                .expect("the watch set answers"),
            "the fixture must owe a backfill for the gate to mean anything"
        );
        assert!(
            index
                .watches
                .observed_from_the_first_block(&owner)
                .expect("the watch set answers"),
            "the birthday gate must be a no-op, or it would weaken the claim \
             whether or not this gate exists"
        );

        assert_eq!(page.completeness, crate::Completeness::Incomplete);
    }

    /// The other gate, and the other axis: a page claiming to hold every
    /// transaction the address ever made is refused outright for an
    /// observation born above the chain's first block, because the range below
    /// a birthday is one no backfill will ever read.
    ///
    /// The observation owes no backfill here — it begins above the line — so
    /// the backfill gate is the no-op and this one carries the whole refusal.
    #[test]
    fn a_history_page_never_claims_a_whole_history_it_has_no_birthday_for() {
        let index = claiming(
            crate::Completeness::CompleteHistory,
            CanonicalPage {
                checkpoint: Some(block(3)),
                transactions: vec![transaction(&network(), 2)],
                next: None,
                completeness: crate::Completeness::Incomplete,
            },
        );
        let owner = address("owner");
        let page = read_for(&index, BlockPosition(4));
        assert!(
            !index
                .watches
                .backfilling(&owner, Some(&block(3)))
                .expect("the watch set answers"),
            "the backfill gate must be a no-op, or it would weaken the claim \
             whether or not this gate exists"
        );

        assert_eq!(page.completeness, crate::Completeness::Incomplete);
    }

    /// And the gates are checks rather than a blanket refusal: an observation
    /// that began at the first block and owes nothing carries the store's claim
    /// through untouched.
    ///
    /// Without this the two above would pass under a reader that reported
    /// `Incomplete` unconditionally, which is exactly the state they were left
    /// in before.
    ///
    /// Owing nothing is spelled as a network with no committed line, because
    /// that is the one way to reach it without a commit: `caught_up` is what
    /// both gates ask, and a network with no checkpoint has no line for an
    /// observation to be behind (`WatchProgress::caught_up`). The
    /// backfill-and-then-catch-up spelling of the same state is
    /// `backfill_contract.rs`, over a store that commits.
    #[test]
    fn a_history_page_keeps_a_claim_both_gates_have_nothing_against() {
        let index = claiming(
            crate::Completeness::CompleteHistory,
            CanonicalPage {
                checkpoint: None,
                transactions: Vec::new(),
                next: None,
                completeness: crate::Completeness::Incomplete,
            },
        );

        assert_eq!(
            read_for(&index, BlockPosition(0)).completeness,
            crate::Completeness::CompleteHistory,
        );
    }

    /// The third narrowing, and the only one that asks the watch set nothing:
    /// `VerifiedOpening` is an independent node read agreeing with the fold
    /// about a *total* at one block, and a page reports which transactions an
    /// address took part in and no total at all. So the evidence has nothing on
    /// a page to be evidence about, and the page answers the range question it
    /// was asked.
    ///
    /// The fixture is the one the scenario above proved lets a claim through
    /// untouched: the observation begins at the chain's first block and owes no
    /// backfill, which the two assertions below re-check on this very read. Both
    /// gates are therefore provably no-ops here, so nothing but this narrowing
    /// can weaken the store's claim — and either gate, left to weaken it,
    /// would make this pass whether the narrowing existed or not
    /// (`while_backfilling` maps `VerifiedOpening` to `Incomplete` too).
    #[test]
    fn a_history_page_never_carries_evidence_about_an_amount_it_omits() {
        let index = claiming(
            crate::Completeness::VerifiedOpening,
            CanonicalPage {
                checkpoint: None,
                transactions: Vec::new(),
                next: None,
                completeness: crate::Completeness::Incomplete,
            },
        );
        let owner = address("owner");
        let page = read_for(&index, BlockPosition(0));
        assert!(
            !index
                .watches
                .backfilling(&owner, None)
                .expect("the watch set answers"),
            "the backfill gate must be a no-op, or it would narrow this claim \
             whether or not the third narrowing exists"
        );
        assert!(
            index
                .watches
                .observed_from_the_first_block(&owner)
                .expect("the watch set answers"),
            "the birthday gate must be a no-op, for the same reason"
        );

        assert_eq!(page.completeness, crate::Completeness::Incomplete);
    }

    #[test]
    fn a_committed_row_is_confirmed_and_counts_depth_from_the_page_checkpoint() {
        let page = CanonicalPage {
            checkpoint: Some(block(3)),
            transactions: vec![transaction(&network(), 2)],
            next: None,
            completeness: crate::Completeness::Incomplete,
        };
        let result = reader(&index(page.clone()))
            .observe(&query(None), page, &[])
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
    fn depth_counts_from_the_pending_tip_once_the_overlay_holds_blocks() {
        let page = CanonicalPage {
            checkpoint: Some(block(3)),
            transactions: vec![transaction(&network(), 2)],
            next: None,
            completeness: crate::Completeness::Incomplete,
        };
        let pending = [admitted(4, 5, "owner"), admitted(5, 7, "owner")];
        let result = reader(&index(page.clone()))
            .observe(&query(None), page, &pending)
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
        let page = CanonicalPage {
            checkpoint: Some(block(3)),
            transactions: Vec::new(),
            next: None,
            completeness: crate::Completeness::Incomplete,
        };
        let after = crate::HistoryPosition {
            height: BlockHeight(2),
            transaction_order: 0,
            transaction: TransactionRef::new(network(), "tx"),
        };
        let result = reader(&index(page.clone()))
            .observe(&query(Some(after)), page, &[admitted(4, 5, "owner")])
            .expect("second page");
        assert!(result.pending.is_empty());
    }

    #[test]
    fn pending_value_reports_both_directions_without_netting_them() {
        let pending = [admitted(4, 5, "owner"), admitted(5, 7, "payer")];
        let value = PendingValue::fold(&query(None), &pending);
        assert_eq!(value.incoming, Decimal::from(5_u64));
        assert_eq!(value.outgoing, Decimal::zero());
        assert_eq!(value.block, Some(block(5)));

        let payer = HistoryQuery {
            address: address("payer"),
            ..query(None)
        };
        let value = PendingValue::fold(&payer, &pending);
        assert_eq!(value.incoming, Decimal::from(7_u64));
        assert_eq!(value.outgoing, Decimal::from(12_u64));
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
            let page = CanonicalPage {
                checkpoint,
                transactions: vec![transaction(&network(), height)],
                next: None,
                completeness: crate::Completeness::Incomplete,
            };
            let error = reader(&index(page.clone()))
                .observe(&query(None), page, &[])
                .expect_err("invalid included history");
            assert_eq!(error.kind, IndexErrorKind::Store);
            assert_eq!(error.message, message);
            assert!(!error.retryable);
        }
    }

    #[test]
    fn failed_history_keeps_its_existing_passthrough_semantics() {
        let mut failed = transaction(&network(), 2);
        failed.status = CanonicalStatus::Failed {
            block: block(2),
            reason: Some("reverted".into()),
        };
        let page = CanonicalPage {
            checkpoint: None,
            transactions: vec![failed],
            next: None,
            completeness: crate::Completeness::Incomplete,
        };
        let observed = reader(&index(page.clone()))
            .observe(&query(None), page, &[])
            .expect("failed observation");
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

    /// Which half of an interleaved commit a durable read sees.
    ///
    /// A commit is not one instant even inside the pass: storage takes
    /// the block first, and the overlay drops it afterwards. A read can land
    /// in either half.
    #[derive(Clone, Copy, Debug)]
    enum Landing {
        /// The store answered from before the commit, and the overlay dropped
        /// the block while that answer was on its way.
        Dropped,
        /// The store answered from after the commit, while the overlay still
        /// held the block.
        Held,
    }

    /// Everything the store holds at one checkpoint.
    #[derive(Clone)]
    struct Stored {
        checkpoint: BlockRef,
        outputs: Vec<crate::IndexedOutput>,
        transactions: Vec<CanonicalTransaction>,
        amount: u64,
    }

    /// A store that lands the pending block's commit *during* its own read.
    ///
    /// The race written down: no sleep, no second thread, no scheduler. The
    /// commit happens at the one instant a reader cannot observe, and which
    /// half of it the read returns is the fixture's only parameter.
    ///
    /// `Clone` because a reader is handed the repository rather than the
    /// service; every clone shares the one overlay the commit lands in, which is
    /// the only state the race is about.
    #[derive(Clone)]
    struct Interleaved {
        landing: Landing,
        overlay: Arc<Frontier>,
        before: Stored,
        after: Stored,
    }

    impl Interleaved {
        fn new(landing: Landing, overlay: Arc<Frontier>) -> Self {
            Self {
                landing,
                overlay,
                before: Stored {
                    checkpoint: block(3),
                    outputs: vec![funding()],
                    transactions: vec![transaction(&network(), 2)],
                    amount: 100,
                },
                after: Stored {
                    checkpoint: block(4),
                    outputs: vec![change()],
                    transactions: vec![transaction(&network(), 2), committed()],
                    amount: 110,
                },
            }
        }

        /// The store's answer, with the commit landing around it.
        fn read(&self) -> &Stored {
            match self.landing {
                Landing::Dropped => {
                    self.overlay
                        .committed(&block(4))
                        .expect("the overlay drops the block storage just took");
                    &self.before
                }
                Landing::Held => &self.after,
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
                checkpoint: Some(stored.checkpoint.clone()),
                transactions: stored.transactions.clone(),
                next: None,
                completeness: crate::Completeness::Incomplete,
            };
            Box::pin(async move { Ok(page) })
        }
    }

    impl BalanceStore for Interleaved {
        fn balance<'a>(
            &'a self,
            _query: crate::BalanceQuery,
        ) -> BoxFuture<'a, Result<Balance, IndexError>> {
            let stored = self.read();
            let balance = Balance {
                amount: Decimal::from(stored.amount),
                observed_at: Some(stored.checkpoint.clone()),
                pending: PendingValue::none(),
                completeness: crate::Completeness::Incomplete,
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

    fn output(id: &str, height: u64, amount: u64) -> crate::IndexedOutput {
        crate::IndexedOutput {
            id: crate::OutputId {
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
    fn funding() -> crate::IndexedOutput {
        output("funding", 2, 100)
    }

    /// The output block four creates.
    fn change() -> crate::IndexedOutput {
        output("spend", 4, 110)
    }

    /// Block four's transaction, as the store holds it once committed.
    fn committed() -> CanonicalTransaction {
        CanonicalTransaction {
            transaction_id: TransactionRef::new(network(), "tx-4"),
            ..transaction(&network(), 4)
        }
    }

    /// Block four, as the overlay holds it: one credit, one spend, one change
    /// output.
    fn spending() -> CanonicalBlock {
        let network = network();
        let draft = crate::ObservationDraft {
            transaction_id: TransactionRef::new(network.clone(), "tx-4"),
            transaction_order: 0,
            status: crate::ObservationDraftStatus::Included,
            movements: vec![ValueMovement::transfer(
                MovementId("0".into()),
                AssetId::native(network.chain.clone()),
                Decimal::from(10_u64),
                address("payer"),
                address("owner"),
            )],
            fee: None,
        };
        CanonicalBlock::new(
            network,
            InterpretedBlock {
                block: block(4),
                transactions: vec![draft],
                outputs: OutputChanges {
                    created: vec![change()],
                    spent: vec![funding().key()],
                    tracked_spends: Vec::new(),
                },
                credits: Vec::new(),
            },
        )
        .expect("a valid pending block")
    }

    fn interleaved(landing: Landing) -> Arc<Service<(), Chain, Interleaved>> {
        let overlay = Arc::new(Frontier::default());
        overlay.admit(spending(), 0).expect("a held block");
        Arc::new(service(Interleaved::new(landing, overlay.clone()), overlay))
    }

    /// The whole point of the overlay: an output the pending block spends is
    /// never offered as spendable, and the output it creates is offered once,
    /// whichever half of the commit the store read caught.
    #[test]
    fn a_commit_landing_inside_a_read_neither_resurrects_a_spend_nor_duplicates_a_change() {
        for landing in [Landing::Dropped, Landing::Held] {
            let index = interleaved(landing);
            let page = futures_executor::block_on(index.outputs().list(OutputRequest {
                network: network(),
                address: address("owner"),
                after: None,
                limit: 10,
            }))
            .expect("live outputs");

            assert_eq!(
                page.outputs
                    .iter()
                    .map(|output| output.id.transaction.id.to_string())
                    .collect::<Vec<_>>(),
                ["spend"],
                "{landing:?}: the spent output is gone and the change is listed once"
            );
        }
    }

    /// The two halves of a balance are one number: a credit the commit moved
    /// across the line is counted exactly once, on whichever side holds it.
    #[test]
    fn a_commit_landing_inside_a_read_moves_a_credit_across_the_line_without_losing_it() {
        for landing in [Landing::Dropped, Landing::Held] {
            let owner = crate::Subject {
                address: address("owner"),
                asset: AssetId::native(network().chain),
            };
            let index = interleaved(landing);
            let balance =
                futures_executor::block_on(index.balances().balance(&owner)).expect("balance");

            assert_eq!(
                Decimal::total([&balance.amount, &balance.pending.incoming]),
                Decimal::from(110_u64),
                "{landing:?}: settled plus pending is the same total either way"
            );
            assert_eq!(balance.pending.outgoing, Decimal::zero());
        }
    }

    /// And the same transaction is reported once, as confirmed or as pending,
    /// never twice and never not at all.
    #[test]
    fn a_commit_landing_inside_a_read_reports_its_transaction_exactly_once() {
        for landing in [Landing::Dropped, Landing::Held] {
            let owner = crate::Subject {
                address: address("owner"),
                asset: AssetId::native(network().chain),
            };
            let index = interleaved(landing);
            let page =
                futures_executor::block_on(reader(&index).history(&owner, crate::Page::first(10)))
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

    #[test]
    fn history_rejects_an_asset_of_another_chain() {
        let foreign = crate::Subject {
            address: address("owner"),
            asset: AssetId::native(ChainName::new("other")),
        };
        let index = index(CanonicalPage {
            checkpoint: None,
            transactions: Vec::new(),
            next: None,
            completeness: crate::Completeness::Incomplete,
        });
        let error =
            futures_executor::block_on(reader(&index).history(&foreign, crate::Page::first(10)))
                .unwrap_err();
        assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
    }

    /// A store that pages its live outputs the way the sqlite one does: the
    /// cursor names how far into the list the last page reached, and the
    /// checkpoint never moves, so nothing but the overlay can change under a
    /// paginated read.
    #[derive(Clone)]
    struct Paged {
        checkpoint: BlockRef,
        outputs: Vec<crate::IndexedOutput>,
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

    /// An index over two stored outputs, one of which `spending()` spends.
    /// The output reader over a store holding two pages, as a consumer holds one:
    /// built once, and paged through twice.
    ///
    /// The service it was built from is dropped before the first page is asked
    /// for, which is the shape of the change that made this a reader: what it
    /// holds is the store, the overlay the caller still owns, and the watch set
    /// — never the value that advances them.
    fn paged(overlay: &Arc<Frontier>) -> ObservedOutputs<Paged> {
        service(
            Paged {
                checkpoint: block(3),
                outputs: vec![funding(), output("second", 2, 50)],
            },
            overlay.clone(),
        )
        .outputs()
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
    /// output from the page in front of it, so without the overlay the cursor
    /// names the second page appends the change and the caller holds the input
    /// *and* the change that exists only because that input was spent — the one
    /// transaction coin selection must never assemble. Nothing else on the path
    /// can catch it: the durable checkpoint is the same on both pages, which is
    /// what the store and `UtxoReader` both validate.
    #[test]
    fn a_pending_block_admitted_between_two_output_pages_refuses_the_rest_of_the_read() {
        let overlay = Arc::new(Frontier::default());
        let index = paged(&overlay);

        let first = futures_executor::block_on(index.list(one(None))).expect("the first page");
        assert_eq!(listed(&first), ["funding"]);
        let cursor = first.next.expect("a second page");

        overlay
            .admit(spending(), 0)
            .expect("a pending block spending the output page one returned");

        let error = futures_executor::block_on(index.list(one(Some(cursor))))
            .expect_err("the rest of the read is refused");
        assert_eq!(error.kind, IndexErrorKind::Conflict);
        assert_eq!(error.message, "pending blocks changed during pagination");
        assert!(
            error.retryable,
            "the caller restarts the read, as it already does when the checkpoint moves"
        );
    }

    /// And an overlay that does not move pages to the end as before: the revision
    /// the cursor names survives the round trip through the store, the spend
    /// applies on the page that holds it, and the change is appended once.
    #[test]
    fn an_unchanged_overlay_pages_to_the_end_and_appends_its_change_once() {
        let overlay = Arc::new(Frontier::default());
        overlay.admit(spending(), 0).expect("a held block");
        let index = paged(&overlay);

        let first = futures_executor::block_on(index.list(one(None))).expect("the first page");
        assert!(
            listed(&first).is_empty(),
            "the pending block spent the only output on this page"
        );
        let cursor = first.next.expect("a second page");
        let second = futures_executor::block_on(index.list(one(Some(cursor))))
            .expect("the last page over the same overlay");

        assert_eq!(listed(&second), ["second", "spend"]);
        assert_eq!(second.next, None);
    }

    /// The cursor this index hands out is the store's own with one field added:
    /// the backend position is byte-for-byte what the store issued, and the
    /// overlay the page was folded over is *named* rather than packed into those
    /// bytes. Which is the whole of what the caller carries back.
    #[test]
    fn an_issued_output_cursor_keeps_the_store_position_and_names_the_overlay() {
        let overlay = Arc::new(Frontier::default());
        overlay.admit(spending(), 0).expect("a held block");
        let held = overlay
            .sampled()
            .expect("the overlay this page folds")
            .revision;
        let index = paged(&overlay);

        let cursor = futures_executor::block_on(index.list(one(None)))
            .expect("the first page")
            .next
            .expect("a second page");

        assert_eq!(cursor.position, vec![1], "the store's own resume position");
        assert_eq!(cursor.checkpoint, Some(block(3)));
        assert_eq!(cursor.revision, Some(held));
    }

    /// A cursor this index did not issue names no overlay to check against, and a
    /// read that trusted it would page the store over an overlay of its own
    /// choosing. It is the store's own refusal, for the same reason.
    #[test]
    fn an_output_cursor_this_index_did_not_issue_is_refused() {
        let index = paged(&Arc::new(Frontier::default()));
        let foreign = OutputCursor {
            checkpoint: Some(block(3)),
            position: vec![1],
            revision: None,
        };

        let error = futures_executor::block_on(index.list(one(Some(foreign))))
            .expect_err("a cursor from another store");

        assert_eq!(error.kind, IndexErrorKind::Store);
        assert_eq!(
            error.message,
            "an output cursor does not belong to this index"
        );
        assert!(!error.retryable);
    }
}
