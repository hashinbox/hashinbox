//! Opening evidence is a conditional claim, never an update to indexed money.

use super::balance::BalanceContract;
use indexing::{
    BalanceEvidence, BalanceQuery, BalanceStore, BlockPosition, BoxFuture, Completeness, Decimal,
    IndexError, IndexErrorKind, IndexStore, IndexedBatch, MovementId, ObservationDraft,
    ObservationDraftStatus, Progress, Purging, ValueMovement, Watch, WatchStore,
};

pub trait EvidenceStore {
    fn attest(&self, evidence: BalanceEvidence) -> BoxFuture<'_, Result<(), IndexError>>;
}
impl EvidenceStore for indexing::memory::Repository {
    fn attest(&self, evidence: BalanceEvidence) -> BoxFuture<'_, Result<(), IndexError>> {
        Box::pin(self.record_evidence(evidence))
    }
}
impl EvidenceStore for indexing::sqlite::Repository {
    fn attest(&self, evidence: BalanceEvidence) -> BoxFuture<'_, Result<(), IndexError>> {
        Box::pin(self.record_evidence(evidence))
    }
}

impl<R: BalanceStore + IndexStore + WatchStore + EvidenceStore + Purging> BalanceContract<'_, R> {
    async fn evidence_fixture(&self) -> (Watch, BalanceQuery, BalanceEvidence) {
        self.prepare().await;
        let f = &self.fixture;
        let watch = Watch {
            id: "opening".into(),
            address: f.address("a"),
            asset: f.native(),
            start: BlockPosition(9),
        };
        f.repository.add(vec![watch.clone()]).await.expect("watch");
        let query = BalanceQuery::new(&watch);
        let balance = f.repository.balance(query.clone()).await.expect("balance");
        let evidence = BalanceEvidence::capture(&query, &balance).expect("capture");
        f.repository.attest(evidence.clone()).await.expect("record");
        (watch, query, evidence)
    }

    pub async fn opening_evidence_requires_the_exact_caught_up_snapshot(&self) {
        let (watch, query, evidence) = self.evidence_fixture().await;
        let f = &self.fixture;
        f.repository
            .attest(evidence.clone())
            .await
            .expect("exact retry");
        let balance = f.repository.balance(query.clone()).await.expect("balance");
        assert_eq!(balance.amount, Decimal::from(7_u64));
        assert_eq!(balance.completeness, Completeness::Incomplete);
        assert_eq!(balance.opening, Some(evidence.clone()));
        let mut changed = balance.clone();
        changed.amount = Decimal::from(8_u64);
        self.refuses(BalanceEvidence::capture(&query, &changed).expect("capture changed amount"))
            .await;
        changed = balance.clone();
        changed
            .observed_at
            .as_mut()
            .expect("checkpoint")
            .hash
            .0
            .push(99);
        self.refuses(BalanceEvidence::capture(&query, &changed).expect("capture changed hash"))
            .await;
        let mut missing = query.clone();
        missing.address = f.address("not-watched");
        self.refuses(BalanceEvidence::capture(&missing, &balance).expect("capture wrong holding"))
            .await;
        let sibling = Watch {
            id: "older-token".into(),
            asset: f.token(),
            start: BlockPosition(0),
            ..watch.clone()
        };
        f.repository
            .add(vec![sibling.clone()])
            .await
            .expect("sibling backfill");
        assert!(
            f.repository
                .balance(query.clone())
                .await
                .expect("hidden")
                .opening
                .is_none()
        );
        self.refuses(evidence.clone()).await;
        f.repository
            .forget(&f.network, &sibling)
            .await
            .expect("forget sibling");
        assert_eq!(
            f.repository
                .balance(query)
                .await
                .expect("unchanged")
                .opening,
            Some(evidence)
        );
        let deficit = Watch {
            id: "deficit".into(),
            address: f.address("d"),
            ..watch
        };
        f.repository
            .add(vec![deficit.clone()])
            .await
            .expect("deficit watch");
        let query = BalanceQuery::new(&deficit);
        let mut balance = f.repository.balance(query.clone()).await.expect("deficit");
        assert_eq!(balance.amount, Decimal::zero());
        assert_eq!(balance.completeness, Completeness::Overdrawn);
        assert!(BalanceEvidence::capture(&query, &balance).is_err());
        balance.completeness = Completeness::Incomplete;
        self.refuses(BalanceEvidence::capture(&query, &balance).expect("forged zero"))
            .await;
    }

    async fn refuses(&self, evidence: BalanceEvidence) {
        let error = self
            .fixture
            .repository
            .attest(evidence)
            .await
            .expect_err("conflict");
        assert_eq!(error.kind, IndexErrorKind::Conflict);
        assert!(error.retryable);
    }

    pub async fn opening_evidence_follows_progress_and_is_invalidated_by_new_old_effects(&self) {
        let (watch, query, evidence) = self.evidence_fixture().await;
        let f = &self.fixture;
        let block = Self::credited(f);
        let batch = IndexedBatch::new(
            f.network.clone(),
            Some(f.block(8)),
            vec![block.clone()],
            vec![Progress {
                watch_id: watch.id.clone(),
                expected: None,
                indexed_through: f.block(9),
            }],
            f.settlement,
        )
        .expect("batch");
        f.repository.commit(batch).await.expect("forward");
        let balance = f
            .repository
            .balance(query.clone())
            .await
            .expect("preserved");
        assert_eq!(balance.amount, Decimal::from(18_u64));
        assert_eq!(balance.opening, Some(evidence.clone()));
        f.repository
            .commit(f.batch(Some(f.block(9)), block))
            .await
            .expect("replay");
        assert_eq!(
            f.repository
                .balance(query.clone())
                .await
                .expect("replay kept")
                .opening,
            Some(evidence.clone())
        );
        self.refuses(evidence).await;
        let self_transfer = ObservationDraft {
            transaction: indexing::Transaction {
                transaction_id: f.transaction("new-old-self"),
                movements: vec![ValueMovement::transfer(
                    MovementId("circle".into()),
                    f.native(),
                    Decimal::from(1_u64),
                    f.address("a"),
                    f.address("a"),
                )],
                fee: None,
            },
            transaction_order: 9,
            status: ObservationDraftStatus::Included,
        };
        let older = f.canonical_block(7, vec![self_transfer]);
        f.repository
            .commit(f.batch(Some(f.block(9)), older))
            .await
            .expect("new historical pair");
        let balance = f
            .repository
            .balance(query.clone())
            .await
            .expect("invalidated");
        assert_eq!(
            balance.amount,
            Decimal::from(18_u64),
            "the novel pair has zero net"
        );
        assert!(balance.opening.is_none());
        let evidence = BalanceEvidence::capture(&query, &balance).expect("recapture");
        f.repository
            .attest(evidence.clone())
            .await
            .expect("record again");
        let renewed = Watch {
            start: BlockPosition(10),
            ..watch.clone()
        };
        f.repository
            .cancel_backfill(vec![renewed.clone()])
            .await
            .expect("reanchor");
        assert!(
            f.repository
                .balance(query.clone())
                .await
                .expect("reset")
                .opening
                .is_none()
        );
        f.repository
            .attest(evidence.clone())
            .await
            .expect("new evidence for renewed row");
        f.repository
            .forget(&f.network, &renewed)
            .await
            .expect("unwatch");
        f.repository.add(vec![renewed]).await.expect("rewatch");
        assert!(
            f.repository
                .balance(query.clone())
                .await
                .expect("no inheritance")
                .opening
                .is_none()
        );
        f.repository
            .attest(evidence)
            .await
            .expect("record before purge");
        let mut other = watch.clone();
        other.id = "other-network".into();
        other.address.network.name.push_str("-utxo");
        f.repository
            .add(vec![other.clone()])
            .await
            .expect("other watch");
        let other_query = BalanceQuery::new(&other);
        let other_balance = f
            .repository
            .balance(other_query.clone())
            .await
            .expect("other balance");
        let other_evidence =
            BalanceEvidence::capture(&other_query, &other_balance).expect("other capture");
        f.repository
            .attest(other_evidence.clone())
            .await
            .expect("other record");
        f.repository.purge_network(&f.network).await.expect("purge");
        assert_eq!(
            f.repository
                .balance(other_query)
                .await
                .expect("other survives")
                .opening,
            Some(other_evidence)
        );
        let empty = f.repository.balance(query).await.expect("purged");
        assert!(empty.opening.is_none());
        assert_eq!(empty.amount, Decimal::zero());
    }
    pub async fn a_forward_deficit_cannot_resurrect_opening_evidence(&self) {
        let (watch, query, _) = self.evidence_fixture().await;
        let f = &self.fixture;
        for (height, from, to) in [(9, "a", "b"), (10, "b", "a")] {
            let draft = ObservationDraft {
                transaction: indexing::Transaction {
                    transaction_id: f.transaction(&format!("change-{height}")),
                    movements: vec![ValueMovement::transfer(
                        MovementId("change".into()),
                        f.native(),
                        Decimal::from(20_u64),
                        f.address(from),
                        f.address(to),
                    )],
                    fee: None,
                },
                transaction_order: 0,
                status: ObservationDraftStatus::Included,
            };
            let batch = IndexedBatch::new(
                f.network.clone(),
                Some(f.block(height - 1)),
                vec![f.canonical_block(height, vec![draft])],
                vec![Progress {
                    watch_id: watch.id.clone(),
                    expected: (height == 10).then(|| f.block(9)),
                    indexed_through: f.block(height),
                }],
                f.settlement,
            )
            .expect("batch");
            f.repository.commit(batch).await.expect("commit");
            let balance = f.repository.balance(query.clone()).await.expect("balance");
            assert!(balance.opening.is_none());
            if height == 9 {
                assert_eq!(balance.completeness, Completeness::Overdrawn);
            } else {
                assert_eq!(balance.amount, Decimal::from(7_u64));
            }
        }
    }
    pub async fn a_deficit_inside_one_batch_invalidates_opening_evidence(&self) {
        let (watch, query, _) = self.evidence_fixture().await;
        let f = &self.fixture;
        let blocks = [(9, "a", "b"), (10, "b", "a")]
            .into_iter()
            .map(|(height, from, to)| {
                let transaction = indexing::Transaction {
                    transaction_id: f.transaction(&format!("change-{height}")),
                    movements: vec![ValueMovement::transfer(
                        MovementId("change".into()),
                        f.native(),
                        Decimal::from(20_u64),
                        f.address(from),
                        f.address(to),
                    )],
                    fee: None,
                };
                let draft = ObservationDraft {
                    transaction,
                    transaction_order: 0,
                    status: ObservationDraftStatus::Included,
                };
                f.canonical_block(height, vec![draft])
            })
            .collect();
        let batch = IndexedBatch::new(
            f.network.clone(),
            Some(f.block(8)),
            blocks,
            vec![Progress {
                watch_id: watch.id,
                expected: None,
                indexed_through: f.block(10),
            }],
            f.settlement,
        )
        .expect("batch");
        f.repository.commit(batch).await.expect("commit");
        let balance = f.repository.balance(query).await.expect("balance");
        assert_eq!(balance.amount, Decimal::from(7_u64));
        assert!(
            balance.opening.is_none(),
            "a later credit cannot erase an earlier block's deficit"
        );
    }
    pub async fn insertion_order_inside_one_block_keeps_opening_evidence(&self) {
        let (watch, query, evidence) = self.evidence_fixture().await;
        let f = &self.fixture;
        let drafts = [(1, "a", "b"), (0, "b", "a")]
            .into_iter()
            .map(|(order, from, to)| ObservationDraft {
                transaction: indexing::Transaction {
                    transaction_id: f.transaction(&format!("within-{order}")),
                    movements: vec![ValueMovement::transfer(
                        MovementId("change".into()),
                        f.native(),
                        Decimal::from(20_u64),
                        f.address(from),
                        f.address(to),
                    )],
                    fee: None,
                },
                transaction_order: order,
                status: ObservationDraftStatus::Included,
            })
            .collect();
        let batch = IndexedBatch::new(
            f.network.clone(),
            Some(f.block(8)),
            vec![f.canonical_block(9, drafts)],
            vec![Progress {
                watch_id: watch.id,
                expected: None,
                indexed_through: f.block(9),
            }],
            f.settlement,
        )
        .expect("batch");
        f.repository.commit(batch).await.expect("commit");
        let balance = f.repository.balance(query).await.expect("balance");
        assert_eq!(balance.amount, Decimal::from(7_u64));
        assert_eq!(
            balance.opening,
            Some(evidence),
            "only complete block state can disprove evidence"
        );
    }
}
