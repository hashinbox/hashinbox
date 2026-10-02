//! Projection of retained transaction facts and block credits into effects.

use super::Repository;
use crate::storage::Probe;
use crate::{
    Activity, ActivityKind, ActivityPage, ActivityPosition, ActivityQuery, ActivityTransaction,
    CanonicalTransaction, Direction, FeeSettlement, IndexError,
};

impl Repository {
    /// Reads confirmed effects and inspection coverage under one lock.
    ///
    /// # Errors
    /// Invalid holding/page, or a poisoned repository lock.
    pub async fn activity(&self, query: ActivityQuery) -> Result<ActivityPage, IndexError> {
        query.validate()?;
        self.read(|tables| {
            let network = &query.address.network;
            let observed_at = tables.checkpoints.get(network).cloned();
            let completeness = Self::provenance(&query, tables, observed_at.as_ref());
            let settlement = tables
                .settlements
                .get(network)
                .copied()
                .unwrap_or(FeeSettlement::Charged);
            let mut effects = Vec::new();
            for (_, transaction) in tables
                .history
                .iter()
                .filter(|((bound, address, ..), _)| Self::owns(bound, address, &query.address))
            {
                effects.extend(transaction.activity(&query, settlement));
            }
            for ((bound, address, height, _), (position, credit)) in &tables.credits {
                if !Self::owns(bound, address, &query.address) || credit.asset != query.asset {
                    continue;
                }
                effects.push(credit.activity(*height, *position));
            }
            effects.retain(|effect| {
                query
                    .after
                    .as_ref()
                    .is_none_or(|after| effect.position > *after)
            });
            effects.sort_by(|left, right| left.position.cmp(&right.position));
            let (effects, next) =
                Probe::new(query.limit).page(effects, |effect| effect.position.clone());
            Ok(ActivityPage {
                effects,
                next,
                observed_at,
                completeness,
            })
        })
    }
}

impl CanonicalTransaction {
    /// One fact's address-local effects, without cancelling self-transfer sides.
    fn activity(&self, query: &ActivityQuery, settlement: FeeSettlement) -> Vec<Activity> {
        let position = ActivityPosition::transaction(self);
        let mut effects = Vec::new();
        for (order, movement) in self.transaction.movements.iter().enumerate() {
            if movement.asset != query.asset {
                continue;
            }
            for (direction, endpoint, counterparty) in [
                (Direction::Credit, movement.to(), movement.from()),
                (Direction::Debit, movement.from(), movement.to()),
            ] {
                if endpoint != Some(&query.address) {
                    continue;
                }
                effects.push(Activity {
                    position: ActivityPosition {
                        movement_order: order as u64,
                        movement: Some(movement.id.clone()),
                        direction,
                        ..position.clone()
                    },
                    block_position: self.block().position,
                    kind: ActivityKind::Movement(movement.kind()),
                    amount: movement.amount.clone(),
                    counterparty: counterparty.cloned(),
                });
            }
        }
        if let Some(fee) = self.transaction.fee.as_ref().filter(|fee| {
            settlement == FeeSettlement::Charged
                && fee.asset == query.asset
                && fee.payer.as_ref() == Some(&query.address)
        }) {
            effects.push(Activity {
                position: ActivityPosition {
                    movement_order: i64::MAX as u64,
                    direction: Direction::Debit,
                    ..position
                },
                block_position: self.block().position,
                kind: ActivityKind::Fee,
                amount: fee.amount.clone(),
                counterparty: None,
            });
        }
        effects
    }
}

impl crate::BlockCredit {
    fn activity(
        &self,
        height: crate::BlockHeight,
        block_position: crate::BlockPosition,
    ) -> Activity {
        Activity {
            position: ActivityPosition {
                height,
                transaction: None,
                movement_order: 0,
                movement: Some(self.id.clone()),
                direction: Direction::Credit,
            },
            block_position,
            kind: ActivityKind::Withdrawal,
            amount: self.amount.clone(),
            counterparty: None,
        }
    }
}

impl ActivityPosition {
    fn transaction(transaction: &CanonicalTransaction) -> Self {
        Self {
            height: transaction.block().height,
            transaction: Some(ActivityTransaction {
                order: transaction.transaction_order,
                id: transaction.transaction.transaction_id.clone(),
            }),
            movement_order: 0,
            movement: None,
            direction: Direction::Credit,
        }
    }
}
