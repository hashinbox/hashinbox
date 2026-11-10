//! What each network's indexer last wrote: the `network_index` table. The
//! server indexes exactly the networks that have a row.

use std::collections::BTreeMap;

use serde::Serialize;

/// One network's indexing as its indexer last wrote it; the counters since
/// the row was made, across restarts.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub(crate) struct NetworkIndex {
    /// The node's newest block at the indexer's last read; always present,
    /// null before the first.
    #[schema(required)]
    pub(crate) tip: Option<u64>,
    /// `tip - confirmations`; always present.
    #[schema(required)]
    pub(crate) confirmed: Option<u64>,
    /// The last kept block; always present, null: none yet.
    #[schema(required)]
    pub(crate) indexed: Option<u64>,
    /// Unix ms of the indexer's last read whose tip moved; always present.
    #[schema(required)]
    pub(crate) checked_at: Option<u64>,
    /// Blocks kept.
    pub(crate) blocks: u64,
    /// Kept blocks archived because the chain took them back.
    pub(crate) reorgs: u64,
    /// Watches added.
    pub(crate) watched: u64,
    /// Failed steps; why the last failed is the index job's `reason`.
    pub(crate) failures: u64,
}

impl NetworkIndex {
    /// One indexer step, what [`db::save`] writes: positions as `after` has
    /// them, `checked_at` `now`, and each counter what it grew
    /// by since `before` (the stats last saved).
    pub(crate) fn step(
        before: &base::Stats,
        after: &base::Stats,
        confirmations: u64,
        now: u64,
    ) -> Self {
        Self {
            tip: after.tip,
            confirmed: after.tip.map(|tip| tip.saturating_sub(confirmations)),
            indexed: after.last.as_ref().map(|last| last.position),
            checked_at: Some(now),
            blocks: after.blocks.saturating_sub(before.blocks),
            reorgs: after.reorgs.saturating_sub(before.reorgs),
            watched: after.watched.saturating_sub(before.watched),
            failures: after.failures.saturating_sub(before.failures),
        }
    }
}

/// The `network_index` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;
    use diesel::{
        prelude::*,
        sql_types::{BigInt, Nullable, Text},
        sqlite::Sqlite,
    };

    use crate::models::{int, schema::network_index, uint};

    /// One `network_index` row as the file holds it.
    #[derive(Queryable, Selectable)]
    #[diesel(table_name = network_index, check_for_backend(Sqlite))]
    struct Row {
        chain: String,
        network: String,
        tip: Option<i64>,
        confirmed: Option<i64>,
        indexed: Option<i64>,
        checked_at: Option<i64>,
        blocks: i64,
        reorgs: i64,
        watched: i64,
        failures: i64,
    }

    impl TryFrom<Row> for (base::Network, NetworkIndex) {
        type Error = anyhow::Error;

        fn try_from(row: Row) -> anyhow::Result<Self> {
            let key = base::Network::new(base::Chain::new(&row.chain), &row.network);
            let index = NetworkIndex {
                tip: row.tip.map(uint).transpose()?,
                confirmed: row.confirmed.map(uint).transpose()?,
                indexed: row.indexed.map(uint).transpose()?,
                checked_at: row.checked_at.map(uint).transpose()?,
                blocks: uint(row.blocks)?,
                reorgs: uint(row.reorgs)?,
                watched: uint(row.watched)?,
                failures: uint(row.failures)?,
            };
            Ok((key, index))
        }
    }

    /// Adds a row, nothing read yet, for every `enabled` network that has
    /// none; keeps every row already written.
    ///
    /// # Errors
    ///
    /// When the write fails.
    pub(crate) fn create_enabled(connection: &mut SqliteConnection) -> anyhow::Result<()> {
        diesel::sql_query(
            "
            INSERT OR IGNORE INTO network_index (chain, network)
            SELECT chain, network
            FROM network
            WHERE enabled",
        )
        .execute(connection)?;
        Ok(())
    }

    /// Writes one step ([`NetworkIndex::step`]) to `network`'s row: a known
    /// position replaces the stored one (an unknown one keeps it),
    /// `checked_at` moves only with the tip (a node that stopped producing
    /// does not look fresh), and the counters add up.
    ///
    /// # Errors
    ///
    /// When a value is out of range or the write fails.
    pub(crate) fn save(
        connection: &mut SqliteConnection,
        network: &base::Network,
        step: &NetworkIndex,
    ) -> anyhow::Result<()> {
        let position = |value: Option<u64>| value.map(int).transpose();
        diesel::sql_query(
            "
            UPDATE network_index
            SET checked_at = IIF(?1 IS NOT NULL AND ?1 IS NOT tip, ?4, checked_at),
                tip = COALESCE(?1, tip),
                confirmed = COALESCE(?2, confirmed),
                indexed = COALESCE(?3, indexed),
                blocks = blocks + ?5,
                reorgs = reorgs + ?6,
                watched = watched + ?7,
                failures = failures + ?8
            WHERE chain = ?9
              AND network = ?10",
        )
        .bind::<Nullable<BigInt>, _>(position(step.tip)?)
        .bind::<Nullable<BigInt>, _>(position(step.confirmed)?)
        .bind::<Nullable<BigInt>, _>(position(step.indexed)?)
        .bind::<Nullable<BigInt>, _>(position(step.checked_at)?)
        .bind::<BigInt, _>(int(step.blocks)?)
        .bind::<BigInt, _>(int(step.reorgs)?)
        .bind::<BigInt, _>(int(step.watched)?)
        .bind::<BigInt, _>(int(step.failures)?)
        .bind::<Text, _>(network.chain.as_str())
        .bind::<Text, _>(&network.name)
        .execute(connection)?;
        Ok(())
    }

    /// `network`'s confirmed position; `None` while unknown.
    pub(crate) fn find_confirmed(
        connection: &mut SqliteConnection,
        network: &base::Network,
    ) -> QueryResult<Option<i64>> {
        let row = network_index::table
            .filter(network_index::chain.eq(network.chain.as_str()))
            .filter(network_index::network.eq(&network.name))
            .select(network_index::confirmed)
            .first::<Option<i64>>(connection)
            .optional()?;
        Ok(row.flatten())
    }

    /// Every row: the networks the server indexes.
    ///
    /// # Errors
    ///
    /// The read, or a row out of range.
    pub(crate) fn find_all(
        connection: &mut SqliteConnection,
    ) -> anyhow::Result<BTreeMap<base::Network, NetworkIndex>> {
        let rows = network_index::table
            .select(Row::as_select())
            .load(connection)?;
        rows.into_iter().map(TryInto::try_into).collect()
    }
}
