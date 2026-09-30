//! The same effect pages over independent concrete backend implementations.

use super::balance::BalanceContract;
use indexing::{
    ActivityKind, ActivityPage, ActivityQuery, BalanceStore, BoxFuture, Completeness, Decimal,
    Direction, IndexError, IndexStore, MovementId, ObservationDraft, ObservationDraftStatus,
    Subject, ValueMovement,
};

/// Test-only substitution for two concrete optional query methods.
pub trait ActivityRead {
    fn effects(&self, query: ActivityQuery) -> BoxFuture<'_, Result<ActivityPage, IndexError>>;
}
impl ActivityRead for indexing::memory::Repository {
    fn effects(&self, query: ActivityQuery) -> BoxFuture<'_, Result<ActivityPage, IndexError>> {
        Box::pin(self.activity(query))
    }
}
impl ActivityRead for indexing::sqlite::Repository {
    fn effects(&self, query: ActivityQuery) -> BoxFuture<'_, Result<ActivityPage, IndexError>> {
        Box::pin(self.activity(query))
    }
}

impl<R: BalanceStore + IndexStore + ActivityRead> BalanceContract<'_, R> {
    pub async fn activity_pages_preserve_every_effect_and_native_order(&self) {
        self.prepare().await;
        let f = &self.fixture;
        let batch = f.batch(Some(f.block(8)), Self::credited(f));
        f.repository.commit(batch.clone()).await.expect("credits");
        f.repository.commit(batch).await.expect("replay");
        let draft = |id: &str, order| ObservationDraft {
            transaction_id: f.transaction(id),
            transaction_order: order,
            status: ObservationDraftStatus::Included,
            movements: vec![ValueMovement::transfer(
                MovementId("native".into()),
                f.native(),
                Decimal::from(1_u64),
                f.address("b"),
                f.address("a"),
            )],
            fee: None,
        };
        let canonical = f.canonical_block(10, vec![draft("a-last", 8), draft("z-first", 2)]);
        f.repository
            .commit(f.batch(Some(f.block(9)), canonical))
            .await
            .expect("native order");
        let subject = Subject {
            address: f.address("a"),
            asset: f.native(),
        };
        let all = f
            .repository
            .effects(ActivityQuery::new(&subject, 100))
            .await
            .expect("all");
        assert_eq!(all.observed_at, Some(f.block(10)));
        assert_eq!(all.completeness, Completeness::Incomplete);
        let mut query = ActivityQuery::new(&subject, 1);
        let mut paged = Vec::new();
        loop {
            let page = f.repository.effects(query.clone()).await.expect("page");
            assert_eq!(page.observed_at, all.observed_at);
            assert_eq!(page.completeness, all.completeness);
            paged.extend(page.effects);
            let Some(next) = page.next else { break };
            query.after = Some(next);
        }
        assert_eq!(
            paged, all.effects,
            "one-row boundaries lose neither self-transfer side"
        );
        let circle = all
            .effects
            .iter()
            .filter(|effect| {
                effect
                    .position
                    .transaction
                    .as_ref()
                    .is_some_and(|transaction| transaction.id.id.as_str() == "circle")
            })
            .collect::<Vec<_>>();
        assert_eq!(circle.len(), 3, "self credit, self debit, fee");
        assert_eq!(
            (circle[0].position.direction, circle[1].position.direction),
            (Direction::Credit, Direction::Debit)
        );
        assert_eq!(
            (circle[0].amount.clone(), circle[1].amount.clone()),
            (Decimal::from(9_u64), Decimal::from(9_u64))
        );
        let failed = all
            .effects
            .iter()
            .filter(|effect| {
                effect
                    .position
                    .transaction
                    .as_ref()
                    .is_some_and(|transaction| transaction.id.id.as_str() == "failed")
            })
            .collect::<Vec<_>>();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].kind, ActivityKind::Fee);
        assert_eq!(failed[0].amount, Decimal::from(7_u64));
        let last = all
            .effects
            .iter()
            .rev()
            .take(2)
            .map(|effect| {
                effect
                    .position
                    .transaction
                    .as_ref()
                    .expect("transaction")
                    .id
                    .id
                    .as_str()
            })
            .collect::<Vec<_>>();
        assert_eq!(last, ["a-last", "z-first"]);
        let only_credit = Subject {
            address: f.address("v"),
            asset: f.native(),
        };
        let withdrawal = f
            .repository
            .effects(ActivityQuery::new(&only_credit, 1))
            .await
            .expect("transactionless");
        assert_eq!(withdrawal.effects.len(), 1);
        assert_eq!(withdrawal.next, None);
        let credit = &withdrawal.effects[0];
        assert_eq!(credit.kind, ActivityKind::Withdrawal);
        assert_eq!(credit.position.transaction, None);
        assert_eq!(credit.amount, Decimal::from(4_u64));
        assert_eq!(credit.block_position, f.block(9).position);
        let mut foreign = ActivityQuery::new(&only_credit, 1);
        foreign.address.network.name = "empty-sibling".into();
        assert!(
            f.repository
                .effects(foreign)
                .await
                .expect("network isolation")
                .effects
                .is_empty()
        );
        let mut invalid = ActivityQuery::new(&subject, 0);
        assert!(f.repository.effects(invalid.clone()).await.is_err());
        invalid.limit = 1;
        invalid.address.value.clear();
        assert!(f.repository.effects(invalid).await.is_err());
    }
}
