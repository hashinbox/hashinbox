//! The SQLite file's own rows, read with plain SQL: how a write contract
//! observes what a commit stored now that the stores answer no balance or
//! history query (051). The memory store has no such reader, so a scenario
//! run on it checks its writes through the ports alone.

use std::str::FromStr as _;

use base::Decimal;
use indexing::{Address, AssetId};
use num_bigint::{BigInt, Sign};
use sqlx::{Row as _, SqlitePool};

/// A SQLite file's pool, read beside the repository that writes it.
#[derive(Clone)]
pub struct Rows(SqlitePool);

impl Rows {
    #[must_use]
    pub const fn new(pool: SqlitePool) -> Self {
        Self(pool)
    }

    /// The ids of `address`'s stored transactions in `asset`, in history
    /// order: a movement of the asset at the address, or a fee it paid that
    /// failed or is in the asset.
    pub async fn history(&self, address: &Address, asset: &AssetId, limit: usize) -> Vec<String> {
        sqlx::query(
            "SELECT h.transaction_id FROM history_entry h
             WHERE h.chain = ?1 AND h.network = ?2 AND h.address = ?3
               AND (EXISTS (SELECT 1 FROM ledger l
                            WHERE l.chain = h.chain AND l.network = h.network
                              AND l.address = h.address AND l.asset = ?4
                              AND l.transaction_id = h.transaction_id AND l.kind < 5)
                    OR (h.fee_payer = h.address
                        AND (h.execution_status = 1 OR h.fee_asset = ?4)))
             ORDER BY h.block_height, h.transaction_order, h.transaction_id LIMIT ?5",
        )
        .bind(address.network.chain.as_str())
        .bind(&address.network.name)
        .bind(&address.value)
        .bind(&asset.asset)
        .bind(i64::try_from(limit).expect("a small limit"))
        .fetch_all(&self.0)
        .await
        .expect("history rows")
        .iter()
        .map(|row| row.get::<String, _>("transaction_id"))
        .collect()
    }

    #[allow(
        dead_code,
        reason = "shared by two test crates; `backfill_contract` reads no total"
    )]
    /// `address`'s settled total in `asset`: the total kept for an observed
    /// holding, otherwise every `ledger` row folded. Zero when overdrawn.
    pub async fn settled(&self, address: &Address, asset: &AssetId) -> Decimal {
        let bind = |statement| {
            sqlx::query(statement)
                .bind(address.network.chain.as_str())
                .bind(&address.network.name)
                .bind(&address.value)
                .bind(&asset.asset)
        };
        let kept = bind(
            "SELECT balance FROM wallet WHERE chain = ?1 AND network = ?2 AND address = ?3
               AND asset = ?4 AND archived_at IS NULL",
        )
        .fetch_optional(&self.0)
        .await
        .expect("a wallet row")
        .and_then(|row| row.get::<Option<String>, _>("balance"));
        let net = match kept {
            Some(text) => BigInt::from_str(&text).expect("an atomic total"),
            None => bind(
                "SELECT direction, amount FROM ledger
                 WHERE chain = ?1 AND network = ?2 AND address = ?3 AND asset = ?4",
            )
            .fetch_all(&self.0)
            .await
            .expect("ledger rows")
            .iter()
            .map(|row| {
                let amount = BigInt::from_str(&row.get::<String, _>("amount")).expect("atomic");
                if row.get::<i64, _>("direction") == 0 {
                    amount
                } else {
                    -amount
                }
            })
            .sum(),
        };
        match net.into_parts() {
            (Sign::Minus, _) => Decimal::zero(),
            (_, units) => Decimal::from_atomic(units, 0),
        }
    }
}
