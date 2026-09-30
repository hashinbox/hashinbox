//! One authoritative total, and the three things that must not move it.
//!
//! Split from `balance.rs` by role, as `rollback.rs` and `backfill_fixture.rs`
//! are: that file states what a fold over stored effects must add up to, and
//! this states what must leave that number alone.
//!
//! **A balance is a recomputation over the subject's own rows, and nothing
//! else.** A store is free to answer it any way it likes, but it may not answer
//! it differently depending on how the rows got there or on who is looking:
//!
//! - *Observing a subject changes nothing.* An effect is written for **every**
//!   endpoint of every movement, watched or not, so an address routinely has a
//!   history of its own before anything observes it — from every time it was
//!   somebody else's counterparty, or from a watch that was withdrawn
//!   (`docs/SYSTEM_REQUIREMENTS.md`: `unwatch` deletes no indexed row). Those
//!   rows are the answer both before and after registration. A store keeping a
//!   running total on the observation instead has to seed it from them, and is
//!   short by all of it *for good* if it does not: the backfill reads from the
//!   birthday and never revisits the blocks below it, and re-presenting rows
//!   that are already stored writes nothing new.
//! - *A repeat is worth nothing.* The same batch presented twice, and a backfill
//!   rediscovering a block for a newly watched address, both write rows that are
//!   already there — so neither may move the number. A total derived from the
//!   batch rather than from the rows would count every redo.
//! - *A deficit is reported, not clamped.* Debits exceeding credits is the one
//!   negative proof the rows can give, and it must survive whatever the store
//!   does between the rows and the answer.

use base::Decimal;

use indexing::{
    AssetId, BalanceQuery, BalanceStore, BlockPosition, CanonicalBlock, Completeness, IndexStore,
    IndexedBatch, InterpretedBlock, MovementId, NetworkFee, ObservationDraft,
    ObservationDraftStatus, Progress, TransactionRef, ValueMovement, Watch, WatchStore,
};

use super::{balance::BalanceContract, fixture::Fixture};

impl<R: BalanceStore + IndexStore + WatchStore> BalanceContract<'_, R> {
    /// Observing an address the store already holds rows for reads the same
    /// number a fold over those rows read before the observation existed.
    ///
    /// The fixture's `a` is nobody's watch while the eight blocks are written —
    /// it is a counterparty, a payer and a recipient — so at registration the
    /// store has its whole history and no observation of it. A store that
    /// answers an observed holding from anything but those rows has to start
    /// that answer from them, and this is what catches it if it does not.
    pub async fn an_observed_total_starts_from_the_rows_already_stored(&self) {
        self.prepare().await;
        let folded = self.reads("a").await;
        assert_eq!(
            folded,
            Decimal::from(7_u64),
            "the fold over the eight fixture blocks"
        );

        self.observe("watcher", "a").await;

        assert_eq!(
            self.reads("a").await,
            folded,
            "the observation reads the same number the fold did, because it \
             started from the rows the fold reads",
        );
    }

    /// A commit moves the observed total by exactly what it wrote, and moves it
    /// again by nothing at all when the same batch is presented twice.
    ///
    /// The ninth block credits `a` 11 with no transaction to name it, which is
    /// the balance change a store is most likely to get wrong twice: it is not a
    /// transaction, it has no counterparty, and it reaches the subject through a
    /// different write path than a transfer does.
    pub async fn a_repeated_commit_moves_an_observed_total_by_nothing(&self) {
        self.prepare().await;
        self.observe("watcher", "a").await;
        let before = self.reads("a").await;

        let credited = Self::credited(&self.fixture);
        let batch = self
            .fixture
            .batch(Some(self.fixture.block(8)), credited.clone());
        self.fixture
            .repository
            .commit(batch)
            .await
            .expect("the credit block");
        let after = self.reads("a").await;
        assert_eq!(
            after,
            Decimal::from(18_u64),
            "seven from the transaction blocks and eleven the ninth credited"
        );
        assert_ne!(after, before, "the commit has to have moved it at all");

        // The same facts again, re-presented at the line they now sit below —
        // which is both what a caller that lost the outcome retries and what a
        // backfill does when it rediscovers a block for a newly watched
        // address. Every insert collides, so every insert reports nothing new.
        let redo = self.fixture.batch(Some(self.fixture.block(9)), credited);
        self.fixture
            .repository
            .commit(redo)
            .await
            .expect("the redone credit block");

        assert_eq!(
            self.reads("a").await,
            after,
            "a retried batch moves the total by nothing: every row it writes \
             is a row the store already holds",
        );
    }

    /// Totals retain deficits, self-transfer cancellation and late historical credits.
    pub async fn a_watched_total_preserves_deficits_until_credits_arrive(&self) {
        self.observe("watched-a", "a").await;
        self.observe("watched-d", "d").await;
        self.prepare().await;
        assert_eq!(self.reads("a").await, Decimal::from(7_u64));
        assert_eq!(
            self.proof("d").await,
            (Decimal::zero(), Completeness::Overdrawn)
        );
        self.fixture
            .repository
            .commit(
                self.fixture
                    .batch(Some(self.fixture.block(8)), Self::credited(&self.fixture)),
            )
            .await
            .expect("credit exactly repays the deficit");
        assert_eq!(self.reads("d").await, Decimal::zero());
        assert_ne!(self.proof("d").await.1, Completeness::Overdrawn);
        let historical = CanonicalBlock::new(
            self.fixture.network.clone(),
            InterpretedBlock {
                credits: vec![indexing::BlockCredit {
                    id: MovementId("late-credit".into()),
                    asset: self.fixture.native(),
                    amount: Decimal::from(1_u64),
                    to: self.fixture.address("d"),
                }],
                ..InterpretedBlock::new(&self.fixture.network, self.fixture.block(1), Vec::new())
                    .expect("empty block")
            },
        )
        .expect("historical credit");
        for _ in 0..2 {
            self.fixture
                .repository
                .commit(
                    self.fixture
                        .batch(Some(self.fixture.block(9)), historical.clone()),
                )
                .await
                .expect("historical insertion or replay");
            assert_eq!(self.reads("d").await, Decimal::from(1_u64));
        }
    }

    /// A deficit reads the same whether or not anything observes the subject.
    ///
    /// The fixture's `d` sends 5 and pays a fee of 7 without ever having been
    /// credited, so the stored rows prove the index is missing credits — which
    /// is what `Completeness::Overdrawn` is for, and the only negative proof the
    /// rows can give. A store that narrowed to one netted non-negative number
    /// anywhere between the rows and the answer would have to report zero here
    /// and lose the debit.
    pub async fn an_observed_deficit_reads_as_overdrawn(&self) {
        self.prepare().await;
        let before = self.proof("d").await;
        assert_eq!(
            before,
            (Decimal::zero(), Completeness::Overdrawn),
            "the fold over the fixture proves this subject is short"
        );

        self.observe("short", "d").await;

        assert_eq!(
            self.proof("d").await,
            before,
            "observing a subject cannot change what its own rows prove",
        );
    }

    /// A movement whose identity is the one a fee row carries commits, and the
    /// fee is still charged exactly once.
    ///
    /// A synthetic identity has to be unforgeable, not merely unlikely: a
    /// readable word would collide with a chain that legitimately emitted a
    /// movement under it, and the collision surfaces as the *whole commit*
    /// refused as corruption rather than as a wrong number. The identity a fee
    /// carries is therefore one no interpreter can produce.
    ///
    /// **The movement has to be a debit of the payer for the collision to be
    /// reachable at all.** A store keying its rows by direction as well as by
    /// identity separates a credit from a fee whatever they are called, so a
    /// movement that only credits the address proves nothing about the name. The
    /// ninth block therefore spends *from* `a` under the identity a fee carries,
    /// in the asset the fee is paid in, in the transaction that pays it.
    pub async fn a_movement_named_like_a_fee_still_commits(&self) {
        self.prepare().await;
        let before = self.reads("a").await;

        self.fixture
            .repository
            .commit(
                self.fixture
                    .batch(Some(self.fixture.block(8)), Self::fee_named(&self.fixture)),
            )
            .await
            .expect("a movement whose identity looks like a fee's");

        assert_eq!(
            before,
            Decimal::from(7_u64),
            "the fold over the eight blocks"
        );
        assert_eq!(
            self.reads("a").await,
            Decimal::from(3_u64),
            "the movement spends 3 and the fee charges 1, both of them real, \
             both of them counted — and the commit was not refused",
        );
    }

    /// A birthday at the chain's first block claims nothing until something has
    /// actually been inspected for the observation.
    ///
    /// [`Completeness::CompleteHistory`] says every change the address ever made
    /// is indexed. A birthday of zero is only half of that — it says where
    /// inspection *would* start, not that any of it happened — and a store that
    /// reads it as the whole thing answers authoritatively for a row registered
    /// one instant ago, over blocks it has never looked at. The other half is the
    /// recorded claim, which is `WatchProgress::caught_up`: the same comparison
    /// the pass and the index's own read gate ask.
    ///
    /// Driven entirely through the ports: `add` registers the observation, a
    /// commit carrying one [`Progress`] is the only way to record a claim, and
    /// `balance` is the only way to read what the store concluded.
    pub async fn a_birthday_at_the_first_block_claims_nothing_uninspected(&self) {
        self.prepare().await;
        self.observe("watcher", "a").await;

        let (amount, claimed) = self.proof("a").await;
        assert_eq!(
            claimed,
            Completeness::Incomplete,
            "the observation starts at block zero and nothing has been \
             inspected for it, so it holds no proof about the range at all",
        );

        // The one write that records a claim. The batch carries no block: the
        // eight it would carry are already stored, and the line it leaves behind
        // is the block the claim names, which is what lets the claim point at it.
        let settled = self.fixture.block(8);
        self.fixture
            .repository
            .commit(
                IndexedBatch::new(
                    self.fixture.network.clone(),
                    Some(settled.clone()),
                    Vec::new(),
                    vec![Progress {
                        watch_id: "watcher".to_owned(),
                        expected: None,
                        indexed_through: settled,
                    }],
                    self.fixture.settlement,
                )
                .expect("a claim on the line the batch leaves behind"),
            )
            .await
            .expect("the claim");

        assert_eq!(
            self.proof("a").await,
            (amount, Completeness::CompleteHistory),
            "now both halves hold: observation began at the first block and \
             every block up to the line has been inspected for it",
        );
    }

    /// The settled amount and what the store claims about the history behind it.
    async fn proof(&self, address: &str) -> (Decimal, Completeness) {
        let balance = self
            .fixture
            .repository
            .balance(BalanceQuery {
                address: self.fixture.address(address),
                asset: self.fixture.native(),
            })
            .await
            .expect("the settled balance");
        (balance.amount, balance.completeness)
    }

    /// Registers an observation of `address` in the native asset, from the
    /// chain's first block.
    async fn observe(&self, id: &str, address: &str) {
        self.fixture
            .repository
            .add(vec![Watch {
                id: id.to_owned(),
                address: self.fixture.address(address),
                asset: self.fixture.native(),
                start: BlockPosition(0),
            }])
            .await
            .expect("the observation");
    }

    /// A ninth block whose one movement is named exactly as a fee row is, spends
    /// from the same subject in the same asset, and rides in the very
    /// transaction whose fee that subject pays.
    fn fee_named(fixture: &Fixture<'_, R>) -> CanonicalBlock {
        let native = fixture.native();
        let interpreted = InterpretedBlock::new(
            &fixture.network,
            fixture.block(9),
            vec![ObservationDraft {
                transaction_id: TransactionRef::new(fixture.network.clone(), "named-like-a-fee"),
                transaction_order: 0,
                status: ObservationDraftStatus::Included,
                movements: vec![ValueMovement::transfer(
                    MovementId("fee".into()),
                    native.clone(),
                    Decimal::from(3_u64),
                    fixture.address("a"),
                    fixture.address("b"),
                )],
                fee: Some(NetworkFee {
                    asset: native,
                    amount: Decimal::from(1_u64),
                    payer: Some(fixture.address("a")),
                }),
            }],
        )
        .expect("unique transaction ids");
        CanonicalBlock::new(fixture.network.clone(), interpreted).expect("valid fixture facts")
    }

    /// The settled amount the store answers for `address` in the native asset.
    async fn reads(&self, address: &str) -> Decimal {
        let asset: AssetId = self.fixture.native();
        self.fixture
            .repository
            .balance(BalanceQuery {
                address: self.fixture.address(address),
                asset,
            })
            .await
            .expect("the settled balance")
            .amount
    }
}
