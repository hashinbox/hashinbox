//! The offline-rescan contract.

use base::Decimal;

use indexing::{
    BlockCredit, BlockHeight, BlockPosition, CanonicalBlock, ConfirmedHistory, FeeSettlement,
    IndexedBatch, IndexedOutput, InterpretedBlock, MovementId, Network, OutputChanges, OutputStore,
    Progress, PurgedRows, Purging, Repository, Watch, WatchProgress,
};

use super::fixture::Fixture;

/// [`Purging::purge_network`] checked against one store holding two networks
/// (`docs/design/019` section 5.3).
///
/// Two networks, because the only interesting question is what a purge does
/// *not* touch: the store the quarantined network shares with every other
/// network, and the observations it shares with the application. One repository,
/// because one repository holds every network — the isolation a purge must
/// respect is the row key, not a handle bound to a network.
pub struct PurgeContract<'a, R> {
    purged: Fixture<'a, R>,
    elsewhere: Fixture<'a, R>,
}

impl<'a, R: OutputStore + Purging + Repository + ConfirmedHistory> PurgeContract<'a, R> {
    /// Checks `repository`, which must hold nothing for either network yet.
    #[must_use]
    pub const fn new(repository: &'a R, network: Network, elsewhere: Network) -> Self {
        Self {
            purged: Fixture::new(repository, network),
            elsewhere: Fixture::new(repository, elsewhere),
        }
    }

    /// A purge removes exactly one network's indexing rows and nothing else:
    /// the other network keeps its checkpoint, history, live outputs and
    /// recorded progress, and both networks keep every observation.
    ///
    /// Also checks the boundary that makes the tool safe to hand an operator:
    /// purging an already-purged network removes nothing.
    pub async fn purge_network_removes_only_its_own_network(&self) {
        Self::fill(&self.purged).await;
        Self::fill(&self.elsewhere).await;

        let removed = self
            .purged
            .repository
            .purge_network(&self.purged.network)
            .await
            .expect("the named network is purged");

        assert_eq!(removed.checkpoint, 1, "the checkpoint row is removed");
        assert_eq!(removed.history, 2, "one history entry per fixture block");
        assert_eq!(removed.outputs, 2, "one live output per fixture block");
        assert_eq!(removed.credits, 2, "one block credit per fixture block");
        assert!(
            removed.movements == 0 || removed.movements == 2,
            "a backend either removes one row per movement or nests movements \
             inside the history entry it removed: {removed:?}",
        );
        self.assert_purged().await;
        self.assert_untouched().await;
        assert_eq!(
            self.purged
                .repository
                .purge_network(&self.purged.network)
                .await
                .expect("a second purge is not an error"),
            PurgedRows::default(),
            "an already-purged network has nothing left to remove",
        );
    }

    /// The purged network holds no indexing row, keeps every observation, and
    /// keeps none of their progress.
    async fn assert_purged(&self) {
        assert_eq!(
            self.purged
                .repository
                .checkpoint(&self.purged.network)
                .await
                .expect("tip"),
            None,
            "the next start must find no checkpoint and re-index from the \
             earliest registered start position",
        );
        assert!(Self::history(&self.purged).await.is_empty());
        assert!(Self::outputs(&self.purged).await.is_empty());
        assert_eq!(
            self.observations(&self.purged.network).await,
            [WatchProgress::unread(Self::watch(&self.purged))],
            "the observations are application state and are never purged, \
             while their progress claimed rows this purge removed — and no \
             `WatchStore` method could reset it afterwards, because `add` \
             preserves progress on an exact repeat",
        );
    }

    /// Every fact of the other network survived, its progress included.
    async fn assert_untouched(&self) {
        assert_eq!(
            self.purged
                .repository
                .checkpoint(&self.elsewhere.network)
                .await
                .expect("tip"),
            Some(self.elsewhere.block(2)),
        );
        assert_eq!(Self::history(&self.elsewhere).await, ["block-1", "block-2"],);
        assert_eq!(Self::outputs(&self.elsewhere).await.len(), 2);
        assert_eq!(
            self.observations(&self.elsewhere.network).await,
            [WatchProgress {
                watch: Self::watch(&self.elsewhere),
                indexed_through: Some(self.elsewhere.block(2)),
            }],
            "another network's observation keeps the progress it proved",
        );
    }

    async fn observations(&self, network: &Network) -> Vec<WatchProgress> {
        self.purged
            .repository
            .all(network)
            .await
            .expect("observations")
    }

    /// Two blocks, each with one transaction, one live output and one
    /// transactionless credit, plus one registration: enough of every
    /// collection a purge names to tell an exact purge from an approximate
    /// one.
    async fn fill(fixture: &Fixture<'_, R>) {
        let network = &fixture.network;
        fixture
            .repository
            .add(vec![Self::watch(fixture)])
            .await
            .expect("the observation");
        for height in 1..=2_u64 {
            let block = fixture.block(height);
            let created = fixture.output("owner", &format!("block-{height}"), BlockHeight(height));
            let interpreted = InterpretedBlock {
                block,
                transactions: vec![fixture.paid("owner", BlockHeight(height))],
                outputs: OutputChanges {
                    created: vec![created],
                    ..OutputChanges::default()
                },
                credits: vec![BlockCredit {
                    id: MovementId(format!("credit-{height}")),
                    asset: fixture.native(),
                    amount: Decimal::from(height),
                    to: fixture.address("owner"),
                }],
            };
            let checkpoint = (height > 1).then(|| fixture.block(1));
            let facts =
                CanonicalBlock::new(network.clone(), interpreted).expect("valid fixture facts");
            // Each block also advances the observation, so the purge has a claim
            // to reset as well as rows to remove. There is no separate progress
            // write to make: a claim rides on the block that proved it.
            let proved = vec![Progress {
                watch: Self::watch(fixture).id,
                expected: checkpoint.clone(),
                indexed_through: fixture.block(height),
            }];
            let batch = IndexedBatch::new(
                network.clone(),
                checkpoint,
                vec![facts],
                proved,
                FeeSettlement::Charged,
            )
            .expect("valid fixture block");
            fixture
                .repository
                .commit(batch)
                .await
                .expect("fixture block");
        }
    }

    async fn history(fixture: &Fixture<'_, R>) -> Vec<String> {
        fixture.history("owner", &fixture.native(), 100).await
    }

    async fn outputs(fixture: &Fixture<'_, R>) -> Vec<IndexedOutput> {
        fixture.outputs("owner", 100).await
    }

    /// The one observation each network carries, so a purge has a claim to
    /// reset as well as rows to remove.
    fn watch(fixture: &Fixture<'_, R>) -> Watch {
        let network = &fixture.network;
        Watch {
            id: format!("{}-{}-owner", network.chain, network.name),
            address: fixture.address("owner"),
            asset: fixture.native(),
            start: BlockPosition(1),
        }
    }
}
