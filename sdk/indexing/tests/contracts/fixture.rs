//! The synthetic chain every contract writes its fixture onto, and the two
//! reads each one checks it with.
//!
//! One block per height: the hash is the height's big-endian bytes, the parent
//! is the height before it, the timestamp is `1_000 + height`. Every contract
//! in this module built that chain itself until this file existed — six
//! byte-identical copies of one ledger, which made a contract look as if it
//! depended on its own block shape when none of them does. What a contract
//! owns is the drafts it writes and the guarantee it asserts; the chain
//! underneath them is the same chain.
//!
//! It is not a store and it holds no assertion: a fixture that cannot be
//! written is a broken fixture, so every method here panics rather than
//! returning an error, and every scenario's `assert` stays in its own file.

use base::Decimal;

use indexing::{
    Address, AssetId, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, CanonicalBlock,
    ConfirmedHistory, FeeSettlement, HistoryQuery, IndexStore, IndexedBatch, IndexedOutput,
    InterpretedBlock, MovementId, Network, ObservationDraft, ObservationDraftStatus, OutputId,
    OutputRequest, OutputStore, TransactionRef, ValueMovement,
};

/// One network's fixture over one repository.
pub(super) struct Fixture<'a, R> {
    pub(super) repository: &'a R,
    pub(super) network: Network,
    /// Where this fixture's chain settles its network fee. It belongs to the
    /// fixture rather than to a read because that is where it belongs in the
    /// store: a batch states it once and the rows carry the answer, so a
    /// contract that wants both rules writes two chains rather than reading one
    /// twice.
    pub(super) settlement: FeeSettlement,
}

impl<'a, R> Fixture<'a, R> {
    /// The fixture of `network` over `repository`, on a chain that charges its
    /// fee beside the movements.
    pub(super) const fn new(repository: &'a R, network: Network) -> Self {
        Self::under(repository, network, FeeSettlement::Charged)
    }

    /// The fixture of `network` over `repository`, under the chain rule
    /// `settlement`.
    pub(super) const fn under(
        repository: &'a R,
        network: Network,
        settlement: FeeSettlement,
    ) -> Self {
        Self {
            repository,
            network,
            settlement,
        }
    }

    /// Block `height` of the synthetic chain, parented on `height - 1`.
    pub(super) fn block(&self, height: u64) -> BlockRef {
        let hash = |height: u64| BlockHash(height.to_be_bytes().to_vec());
        BlockRef {
            position: BlockPosition(height),
            height: BlockHeight(height),
            hash: hash(height),
            parent: Some(BlockParent {
                position: BlockPosition(height - 1),
                hash: hash(height - 1),
            }),
            timestamp: Some(1_000 + height),
        }
    }

    pub(super) fn address(&self, value: &str) -> Address {
        Address::new(self.network.clone(), value)
    }

    pub(super) fn transaction(&self, id: &str) -> TransactionRef {
        TransactionRef::new(self.network.clone(), id)
    }

    /// The asset every chain has and every fee is paid in.
    pub(super) fn native(&self) -> AssetId {
        AssetId::native(self.network.chain.clone())
    }

    /// The one non-native asset the fixtures name, so a query over an asset
    /// can be wrong about which one it selected.
    pub(super) fn token(&self) -> AssetId {
        AssetId {
            chain: self.network.chain.clone(),
            asset: "token".into(),
        }
    }

    /// One transaction paying `owner` 5 at `height`, so a fixture block has
    /// history to keep, to fill in, or to unwind.
    pub(super) fn paid(&self, owner: &str, height: BlockHeight) -> ObservationDraft {
        ObservationDraft {
            transaction_id: self.transaction(&format!("block-{}", height.0)),
            transaction_order: 0,
            status: ObservationDraftStatus::Included,
            movements: vec![ValueMovement::mint(
                MovementId(format!("mint-{}", height.0)),
                self.native(),
                Decimal::from(5_u64),
                self.address(owner),
            )],
            fee: None,
        }
    }

    /// One live output of `owner`, created by `transaction` at `created_at`.
    pub(super) fn output(
        &self,
        owner: &str,
        transaction: &str,
        created_at: BlockHeight,
    ) -> IndexedOutput {
        IndexedOutput {
            id: OutputId {
                transaction: self.transaction(transaction),
                index: 0,
            },
            address: self.address(owner),
            asset: self.native(),
            amount: Decimal::from(9_u64),
            evidence: vec![1, 2, 3],
            created_at,
            coinbase: false,
        }
    }

    /// Block `height`, carrying `draft` alone and no output or credit.
    pub(super) fn canonical(&self, height: u64, draft: ObservationDraft) -> CanonicalBlock {
        self.canonical_block(height, vec![draft])
    }

    /// Block `height`, carrying `drafts` and no output or credit.
    ///
    /// The vector is the order the block is *reported* in, which is not the
    /// order it is stored in: each draft names its own
    /// `transaction_order`, so a contract can hand them over shuffled and still
    /// say what the page must look like.
    pub(super) fn canonical_block(
        &self,
        height: u64,
        drafts: Vec<ObservationDraft>,
    ) -> CanonicalBlock {
        let interpreted = InterpretedBlock::new(&self.network, self.block(height), drafts)
            .expect("unique transaction ids");
        CanonicalBlock::new(self.network.clone(), interpreted).expect("valid fixture facts")
    }

    /// One block's facts per draft, heights 1 upward, each connecting to the
    /// one before, so [`Self::commit`] can write the chain in order.
    pub(super) fn chain(&self, drafts: Vec<ObservationDraft>) -> Vec<CanonicalBlock> {
        let heights = 1_u64..;
        heights
            .zip(drafts)
            .map(|(height, draft)| self.canonical(height, draft))
            .collect()
    }

    /// One fixture block as the one write a store takes, extending
    /// `checkpoint` and claiming no progress.
    pub(super) fn batch(
        &self,
        checkpoint: Option<BlockRef>,
        canonical: CanonicalBlock,
    ) -> IndexedBatch {
        IndexedBatch::new(
            self.network.clone(),
            checkpoint,
            vec![canonical],
            Vec::new(),
            self.settlement,
        )
        .expect("a connected fixture block")
    }
}

impl<R: IndexStore> Fixture<'_, R> {
    /// Commits `blocks` in order, each authorised by the one before.
    pub(super) async fn commit(&self, blocks: Vec<CanonicalBlock>) {
        let mut checkpoint = None;
        for canonical in blocks {
            let block = canonical.block().clone();
            self.repository
                .commit(self.batch(checkpoint, canonical))
                .await
                .expect("fixture block");
            checkpoint = Some(block);
        }
    }
}

impl<R: ConfirmedHistory> Fixture<'_, R> {
    /// The ids of every stored transaction of `address` in `asset`, in order.
    pub(super) async fn history(
        &self,
        address: &str,
        asset: &AssetId,
        limit: usize,
    ) -> Vec<String> {
        ConfirmedHistory::history(
            self.repository,
            HistoryQuery {
                address: self.address(address),
                asset: asset.clone(),
                after: None,
                limit,
            },
        )
        .await
        .expect("history page")
        .transactions
        .into_iter()
        .map(|transaction| transaction.transaction_id.id.to_string())
        .collect()
    }
}

impl<R: OutputStore> Fixture<'_, R> {
    /// The first page of up to `limit` of `address`'s live outputs, as a
    /// request.
    pub(super) fn request(&self, address: &str, limit: usize) -> OutputRequest {
        OutputRequest {
            network: self.network.clone(),
            address: self.address(address),
            after: None,
            limit,
        }
    }

    /// Every live output of `address`.
    pub(super) async fn outputs(&self, address: &str, limit: usize) -> Vec<IndexedOutput> {
        OutputStore::list(self.repository, self.request(address, limit))
            .await
            .expect("live outputs")
            .outputs
    }
}
