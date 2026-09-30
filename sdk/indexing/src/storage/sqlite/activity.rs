//! Bounded address/asset effect pages over the existing movement table.

use super::{Repository, Tables, failure, store};
use crate::storage::Probe;
use crate::{
    Activity, ActivityKind, ActivityPage, ActivityPosition, ActivityQuery, ActivityTransaction,
    Address, BlockHeight, BlockPosition, Direction, IndexError, MovementId, TransactionRef,
};
use rusqlite::{Connection, Row, params};

const PAGE: &str = "SELECT m.block_height, t.transaction_order, m.transaction_id, m.movement_order,
                    m.movement_id, m.direction, m.block_position, m.kind, m.amount, m.counterparty
             FROM balance_movement m LEFT JOIN transaction_fact t
               ON t.chain = m.chain AND t.network = m.network
                  AND t.transaction_id = m.transaction_id
             WHERE m.chain = ?1 AND m.network = ?2 AND m.address = ?3 AND m.asset = ?4
               AND (?5 IS NULL OR
                (m.block_height, COALESCE(t.transaction_order, -1), m.transaction_id,
                 m.movement_order, m.movement_id, m.direction) > (?5, ?6, ?7, ?8, ?9, ?10))
             ORDER BY m.block_height, COALESCE(t.transaction_order, -1), m.transaction_id,
                      m.movement_order, m.movement_id, m.direction LIMIT ?11";

impl Repository {
    /// Reads confirmed effects, checkpoint and coverage in one database snapshot.
    ///
    /// # Errors
    /// Invalid holding/page, unsupported cursor coordinates, or a storage failure.
    pub async fn activity(&self, query: ActivityQuery) -> Result<ActivityPage, IndexError> {
        query.validate()?;
        let tables = self.tables(&query.address.network)?;
        self.database
            .blocking(move |connection| tables.activity(connection, &query))
            .await
    }
}

impl Tables {
    fn activity(
        &self,
        connection: &Connection,
        query: &ActivityQuery,
    ) -> Result<ActivityPage, IndexError> {
        let (read, observed_at) = self.snapshot(connection)?;
        let completeness = self.provenance(&read, query, observed_at.as_ref())?;
        let probe = Probe::new(query.limit);
        let after = query.after.as_ref();
        let transaction = after.and_then(|position| position.transaction.as_ref());
        let mut statement = read
            .prepare_cached(PAGE)
            .map_err(|error| failure(&error, "failed to prepare activity page"))?;
        let mut rows = statement
            .query(params![
                self.chain(),
                self.name(),
                query.address.value,
                query.asset.asset,
                after
                    .map(|value| Self::stored(value.height.0))
                    .transpose()?,
                transaction
                    .map(|value| Self::stored(value.order))
                    .transpose()?
                    .unwrap_or(-1),
                transaction.map_or("", |value| value.id.id.as_str()),
                after
                    .map(|value| Self::stored(value.movement_order))
                    .transpose()?
                    .unwrap_or(0),
                after
                    .and_then(|value| value.movement.as_ref())
                    .map_or("", |id| id.0.as_str()),
                i64::from(after.is_some_and(|value| value.direction == Direction::Debit)),
                probe.rows() as i64,
            ])
            .map_err(|error| failure(&error, "failed to read activity page"))?;
        let mut effects = Vec::new();
        while let Some(row) = Self::step(&mut rows)? {
            effects.push(Self::effect(row, query)?);
        }
        let (effects, next) = probe.page(effects, |effect| effect.position.clone());
        Ok(ActivityPage {
            effects,
            next,
            observed_at,
            completeness,
        })
    }

    fn effect(row: &Row<'_>, query: &ActivityQuery) -> Result<Activity, IndexError> {
        let id: String = Self::column(row, 2)?;
        let transaction = if id.is_empty() {
            None
        } else {
            Some(ActivityTransaction {
                order: Self::restored(Self::column(row, 1)?)?,
                id: TransactionRef::new(query.address.network.clone(), id),
            })
        };
        let movement: String = Self::column(row, 4)?;
        let direction = match Self::column::<i64>(row, 5)? {
            0 => Direction::Credit,
            1 => Direction::Debit,
            _ => return Err(store("a stored activity has an unknown direction")),
        };
        let kind = match Self::column::<i64>(row, 7)? {
            0 => ActivityKind::Movement(base::MovementKind::Transfer),
            1 => ActivityKind::Movement(base::MovementKind::Input),
            2 => ActivityKind::Movement(base::MovementKind::Output),
            3 => ActivityKind::Movement(base::MovementKind::Mint),
            4 => ActivityKind::Movement(base::MovementKind::Burn),
            5 => ActivityKind::Fee,
            6 => ActivityKind::Withdrawal,
            _ => return Err(store("a stored activity has an unknown kind")),
        };
        Ok(Activity {
            position: ActivityPosition {
                height: BlockHeight(Self::restored(Self::column(row, 0)?)?),
                transaction,
                movement_order: Self::restored(Self::column(row, 3)?)?,
                movement: (!movement.is_empty()).then_some(MovementId(movement)),
                direction,
            },
            block_position: BlockPosition(Self::restored(Self::column(row, 6)?)?),
            kind,
            amount: Self::decimal(row, 8)?,
            counterparty: Self::column::<Option<String>>(row, 9)?
                .map(|value| Address::new(query.address.network.clone(), value)),
        })
    }
}
