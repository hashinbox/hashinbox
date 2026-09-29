//! The watch-store contract: what every store must do with the list of what to
//! observe, and with the progress recorded against it.
//!
//! Stated entirely in operations and guarantees, so it outlives whichever engine
//! answers it. A store passes when, and only when, it gives an index six things:
//! an atomic batch write; recognition of an exact repeat that leaves recorded
//! progress alone; refusal of a redefinition under a name that is taken; one
//! snapshot pairing each observation with its own coverage; a withdrawal that
//! takes the claim and leaves the facts; and a re-anchor that moves a birthday
//! and discards the claim earned under it without the row ever being absent.

use indexing::{
    Address, AssetId, BlockHash, BlockHeight, BlockPosition, BlockRef, CanonicalBlock,
    FeeSettlement, IndexErrorKind, IndexStore, IndexedBatch, InterpretedBlock, Network,
    OutputChanges, Progress, Watch, WatchProgress, WatchStore,
};

/// The [`WatchStore`] contract, checked against one repository bound to
/// `network`, which must hold nothing for it yet.
///
/// It reaches [`IndexStore`] too, for one reason: recorded progress is written
/// by a commit and never by a setter, so a contract that could not commit could
/// not state what `add` must leave alone.
pub struct WatchContract<'a, R> {
    pub(super) repository: &'a R,
    network: Network,
}

impl<'a, R: WatchStore + IndexStore> WatchContract<'a, R> {
    /// Checks `repository`, which must hold nothing for `network` yet.
    #[must_use]
    pub const fn new(repository: &'a R, network: Network) -> Self {
        Self {
            repository,
            network,
        }
    }

    /// Every field of every observation comes back unchanged, with no progress.
    pub async fn round_trips_every_field(&self) {
        let signing = self.watch("signing", "native", "a", 7);
        let watching = self.watch("watching", "0xtoken", "b", i64::MAX.unsigned_abs());

        self.add(vec![signing.clone(), watching.clone()]).await;

        assert_eq!(
            self.stored().await,
            [
                WatchProgress::unread(signing),
                WatchProgress::unread(watching),
            ],
            "both observations, each with no progress yet: `None` is \"nobody \
             worked out what is owed\", which the index resolves once",
        );
    }

    /// An identity means one observation. Re-presenting it over another
    /// address, asset or birthday is refused, and changes nothing.
    pub async fn rejects_a_redefined_identity(&self) {
        let first = self.watch("wallet", "native", "a", 7);
        self.add(vec![first.clone()]).await;

        for redefined in [
            self.watch("wallet", "0xtoken", "a", 7),
            self.watch("wallet", "native", "elsewhere", 7),
            self.watch("wallet", "native", "a", 9),
        ] {
            let error = self
                .repository
                .add(vec![redefined])
                .await
                .expect_err("a redefinition under a name that is taken");
            assert_eq!(error.kind, IndexErrorKind::Conflict);
        }

        assert_eq!(self.watches().await, [first], "nothing was retargeted");
    }

    /// One address is observed once per asset, and may be observed for several.
    ///
    /// This is the capability the durable schema always had and the hot watch
    /// set used to refuse: a wallet holding a chain's own currency and a token
    /// at one address is two observations with two identities, and one
    /// inspection of that address proves both (`docs/design/030 §8 (ex-032)` section 4).
    pub async fn accepts_one_address_once_per_asset(&self) {
        let native = self.watch("native-wallet", "native", "a", 7);
        let token = self.watch("token-wallet", "0xtoken", "a", 7);

        self.add(vec![native.clone(), token.clone()]).await;

        let error = self
            .repository
            .add(vec![self.watch("second-native", "native", "a", 7)])
            .await
            .expect_err("one address, one asset, one observation");
        assert_eq!(error.kind, IndexErrorKind::Conflict);
        assert_eq!(self.watches().await, [native, token]);
    }

    /// An exact repeat is accepted and leaves recorded progress alone.
    ///
    /// The whole reason `add` recognises a repeat instead of refusing it: a
    /// store failure or a cancelled future leaves the outcome of an `add`
    /// unknown, so the caller repeats it — and a repeat that reset progress
    /// would make every uncertain registration re-read the chain.
    pub async fn preserves_progress_on_an_exact_repeat(&self) {
        let watch = self.watch("wallet", "native", "a", 1);
        self.add(vec![watch.clone()]).await;
        let recorded = self.record(std::slice::from_ref(&watch)).await;

        self.add(vec![watch.clone()]).await;

        assert_eq!(
            self.stored().await,
            [WatchProgress {
                watch,
                indexed_through: Some(recorded)
            }],
            "the repeat kept the progress the store had already proved",
        );
    }

    /// A batch is one write: a conflict anywhere in it leaves no row behind.
    pub async fn writes_a_batch_whole_or_not_at_all(&self) {
        let taken = self.watch("taken", "native", "a", 7);
        self.add(vec![taken.clone()]).await;

        let error = self
            .repository
            .add(vec![
                self.watch("fresh", "native", "b", 7),
                self.watch("taken", "native", "c", 7),
            ])
            .await
            .expect_err("a batch whose second entry redefines an identity");

        assert_eq!(error.kind, IndexErrorKind::Conflict);
        assert_eq!(
            self.watches().await,
            [taken],
            "the first entry of the refused batch was not kept",
        );
    }

    /// A batch names one network. Two would be two writes, and a caller cannot
    /// be told which half landed.
    pub async fn refuses_a_batch_of_two_networks(&self) {
        let elsewhere = Network::new(
            self.network.chain.clone(),
            format!("{}-other", self.network.name),
        );
        let mut foreign = self.watch("foreign", "native", "a", 7);
        foreign.address = Address::new(elsewhere, "a");

        let error = self
            .repository
            .add(vec![self.watch("mine", "native", "b", 7), foreign])
            .await
            .expect_err("a batch spanning two networks");

        assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
        assert!(self.watches().await.is_empty());
    }

    /// A blank address is refused, so restoring the list cannot put one in the
    /// watch set.
    ///
    /// The other half of the reason [`indexing::Watching::watch`] refuses one
    /// synchronously. Those two are the only ways an address enters a watch set
    /// — a caller's request, and a restart reading this list — so between them
    /// the selection a pass reads can hold no blank address, and the pass needs
    /// no check of its own.
    pub async fn refuses_a_blank_address(&self) {
        let mut blank = self.watch("blank", "native", "", 7);
        blank.address = Address::new(self.network.clone(), "");

        let error = self
            .repository
            .add(vec![blank])
            .await
            .expect_err("a blank address");

        assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
        assert!(self.watches().await.is_empty());
    }

    /// A forgotten identity frees both of its uniqueness constraints, and
    /// forgetting one nothing holds is not an error.
    ///
    /// The durable half of an `unwatch`, so what it proves is what makes the act
    /// reversible: the id and the `(address, asset)` pair can both be observed
    /// again, from a different birthday, without the store remembering the row
    /// that was removed — including its coverage, which was a claim about the
    /// withdrawn watch and not about the chain.
    pub async fn forgets_an_identity_and_accepts_it_again(&self) {
        let watch = self.watch("withdrawn", "native", "a", 1);
        self.forget(&watch).await;
        self.add(vec![watch.clone()]).await;
        drop(self.record(std::slice::from_ref(&watch)).await);

        self.forget(&watch).await;
        assert!(self.stored().await.is_empty());

        let again = self.watch("withdrawn", "native", "a", 11);
        self.add(vec![again.clone()]).await;
        assert_eq!(
            self.stored().await,
            [WatchProgress::unread(again)],
            "the same identity and address, from another birthday, owed its \
             range again rather than inheriting the withdrawn claim",
        );
    }

    /// Each observation keeps the progress it earned, and a withdrawal takes
    /// only its own.
    ///
    /// **This is what per-observation progress changed, and it is stronger than
    /// what it replaced.** Progress used to be one row per *address*, shared by
    /// every asset observed at it, so withdrawing either observation raised the
    /// question of who owned the claim — and a second observation registered from
    /// an earlier birthday had to widen the shared range, which a restart in
    /// between could read back as settled (`docs/design/030 §8 (ex-032)`, the first defect).
    /// Now each observation carries its own contiguous claim, earned under its own
    /// birthday: two observations of one address advance together because one
    /// inspection serves both ([`indexing::BlockInterpreter::inspect`] takes
    /// addresses and no asset), not because they share a row.
    pub async fn keeps_the_progress_of_each_observation_apart(&self) {
        let native = self.watch("native-wallet", "native", "a", 1);
        let token = self.watch("token-wallet", "0xtoken", "a", 1);
        self.add(vec![native.clone(), token.clone()]).await;
        let recorded = self.record(&[native.clone(), token.clone()]).await;

        self.forget(&token).await;

        assert_eq!(
            self.stored().await,
            [WatchProgress {
                watch: native.clone(),
                indexed_through: Some(recorded)
            }],
            "the surviving observation keeps its own claim",
        );

        self.forget(&native).await;
        assert!(
            self.stored().await.is_empty(),
            "and the last one out takes its own with it",
        );
    }

    /// A re-anchor raises one observation's birthday, discards the claim it
    /// earned under the old one, and touches no other row.
    ///
    /// **The row never stops existing, which is the whole reason the port has
    /// this operation.** The same end state is reachable by forgetting the
    /// observation and adding it back, and that route passes through one in which
    /// the address is watched by nothing — a state no caller asked for and a
    /// crash can leave behind for good. Repeating the call is therefore also how
    /// an uncertain one is settled, which the second half checks.
    pub async fn reanchors_an_observation_and_discards_its_claim(&self) {
        let native = self.watch("native-wallet", "native", "a", 1);
        let token = self.watch("token-wallet", "0xtoken", "a", 1);
        self.add(vec![native.clone(), token.clone()]).await;
        let recorded = self.record(&[native.clone(), token.clone()]).await;
        let raised = Watch {
            start: BlockPosition(20),
            ..native
        };
        let settled = [
            WatchProgress::unread(raised.clone()),
            WatchProgress {
                watch: token,
                indexed_through: Some(recorded),
            },
        ];

        self.reanchor(vec![raised.clone()]).await;
        assert_eq!(
            self.stored().await,
            settled,
            "the birthday moved, its own claim went, and the other observation \
             of the same address kept the one it earned",
        );

        self.reanchor(vec![raised]).await;
        assert_eq!(
            self.stored().await,
            settled,
            "and a repeat is the same store, which is how a caller that lost the \
             answer to the first one gets a definite state",
        );
    }

    /// A re-anchor states the whole observation, so a row the store has lost
    /// comes back as the one the caller described.
    ///
    /// **This is what makes a cancellation retryable.** The caller that lost the
    /// answer to a re-anchor cannot tell a store that applied it from one that
    /// applied nothing, and there is a third case it cannot tell either: a store
    /// whose row is gone. All three are settled by presenting the same batch
    /// again, because the call says what must be true afterwards rather than what
    /// to change.
    pub async fn reanchors_an_observation_the_store_has_lost(&self) {
        let watch = self.watch("wallet", "native", "a", 1);
        let raised = Watch {
            start: BlockPosition(20),
            ..watch.clone()
        };
        self.add(vec![watch.clone()]).await;
        drop(self.record(std::slice::from_ref(&watch)).await);
        self.forget(&watch).await;

        self.reanchor(vec![raised.clone()]).await;

        assert_eq!(
            self.stored().await,
            [WatchProgress::unread(raised)],
            "the observation the batch describes, at its stated birthday, with \
             nothing inspected for it",
        );
    }

    /// A re-anchor does not take a name that is in use, and is refused whole.
    ///
    /// The `(address, asset)` pair is the row's other name, so re-anchoring one
    /// observation onto the pair another holds would hand it the progress the
    /// other earned — the same reason `add` refuses it. Which name the store
    /// withdraws by is therefore not free: keyed by the pair it would take it
    /// silently, and the two stores would answer differently.
    pub async fn refuses_a_reanchor_onto_a_pair_another_identity_holds(&self) {
        let mine = self.watch("mine", "native", "a", 1);
        let theirs = self.watch("theirs", "native", "b", 1);
        self.add(vec![mine.clone(), theirs.clone()]).await;

        let error = self
            .repository
            .reanchor(vec![Watch {
                address: theirs.address.clone(),
                ..mine.clone()
            }])
            .await
            .expect_err("a re-anchor onto an address another observation holds");

        assert_eq!(error.kind, IndexErrorKind::Conflict);
        assert_eq!(
            self.watches().await,
            [mine, theirs],
            "the refused batch retargeted nothing and withdrew nothing",
        );
    }

    /// One store holds every network, and a read of one never sees another's.
    pub async fn answers_only_its_own_network(&self) {
        let elsewhere = Network::new(
            self.network.chain.clone(),
            format!("{}-other", self.network.name),
        );
        let mut foreign = self.watch("foreign", "native", "a", 7);
        foreign.address = Address::new(elsewhere.clone(), "a");
        let mine = self.watch("mine", "native", "a", 7);

        self.add(vec![mine.clone()]).await;
        self.add(vec![foreign.clone()]).await;

        assert_eq!(self.watches().await, [mine]);
        assert_eq!(
            self.repository
                .all(&elsewhere)
                .await
                .expect("the other network's observations")
                .into_iter()
                .map(|progress| progress.watch)
                .collect::<Vec<_>>(),
            [foreign],
            "each network reads its own rows out of one store",
        );
    }

    /// Records progress for every watch in `watches`, the only way a store
    /// accepts it: inside a commit, against the network's own checkpoint.
    pub(super) async fn record(&self, watches: &[Watch]) -> BlockRef {
        let checkpoint = match self
            .repository
            .checkpoint(&self.network)
            .await
            .expect("the checkpoint")
        {
            Some(checkpoint) => checkpoint,
            None => self.commit_anchor().await,
        };
        let progress = watches
            .iter()
            .map(|watch| Progress {
                watch: watch.id.clone(),
                expected: None,
                indexed_through: checkpoint.clone(),
            })
            .collect();
        let batch = IndexedBatch::new(
            self.network.clone(),
            Some(checkpoint.clone()),
            Vec::new(),
            progress,
            FeeSettlement::Charged,
        )
        .expect("a batch that records progress and writes no block");
        self.repository
            .commit(batch)
            .await
            .expect("recording progress");
        checkpoint
    }

    /// One empty block, so the network has a checkpoint to record against.
    pub(super) async fn commit_anchor(&self) -> BlockRef {
        let block = BlockRef {
            position: BlockPosition(1),
            height: BlockHeight(1),
            hash: BlockHash(vec![1]),
            parent: None,
            timestamp: None,
        };
        let facts = CanonicalBlock::new(
            self.network.clone(),
            InterpretedBlock {
                block: block.clone(),
                transactions: Vec::new(),
                outputs: OutputChanges::default(),
                credits: Vec::new(),
            },
        )
        .expect("valid fixture facts");
        let batch = IndexedBatch::new(
            self.network.clone(),
            None,
            vec![facts],
            Vec::new(),
            FeeSettlement::Charged,
        )
        .expect("the network's first block");
        self.repository
            .commit(batch)
            .await
            .expect("the fixture block");
        block
    }

    pub(super) async fn add(&self, watches: Vec<Watch>) {
        self.repository
            .add(watches)
            .await
            .expect("a batch of observations");
    }

    pub(super) async fn reanchor(&self, watches: Vec<Watch>) {
        self.repository
            .reanchor(watches)
            .await
            .expect("a batch of re-anchored observations");
    }

    pub(super) async fn forget(&self, watch: &Watch) {
        self.repository
            .forget(&self.network, watch)
            .await
            .expect("forgetting is idempotent");
    }

    pub(super) async fn stored(&self) -> Vec<WatchProgress> {
        let mut stored = self
            .repository
            .all(&self.network)
            .await
            .expect("the observations");
        stored.sort_by(|left, right| left.watch.id.cmp(&right.watch.id));
        stored
    }

    pub(super) async fn watches(&self) -> Vec<Watch> {
        self.stored()
            .await
            .into_iter()
            .map(|progress| progress.watch)
            .collect()
    }

    pub(super) fn watch(&self, id: &str, asset: &str, address: &str, start: u64) -> Watch {
        Watch {
            id: format!("{}-{}-{id}", self.network.chain, self.network.name),
            address: Address::new(self.network.clone(), address),
            asset: AssetId {
                chain: self.network.chain.clone(),
                asset: asset.to_owned(),
            },
            start: BlockPosition(start),
        }
    }
}
