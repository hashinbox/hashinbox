//! What each network's indexer last wrote: the `network_index` table. The
//! server indexes exactly the networks that have a row.

use std::collections::BTreeMap;

use serde::Serialize;

/// One network's indexing as its indexer last wrote it; the counters since
/// the row was made, across restarts. Written the first time the tip is
/// known: no row, no tip yet.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub(crate) struct NetworkIndex {
    /// The node's newest block at the indexer's last read.
    pub(crate) tip: u64,
    /// `tip - confirmations` (saturating), written with it.
    pub(crate) confirmed: u64,
    /// The last kept block; always present, null: none yet.
    #[schema(required)]
    pub(crate) indexed: Option<u64>,
    /// Unix ms of the indexer's last read whose tip moved; always present.
    #[schema(required)]
    pub(crate) checked_at: Option<u64>,
    /// Blocks kept.
    pub(crate) blocks: u64,
    /// Kept blocks reorged because the chain took them back.
    pub(crate) reorgs: u64,
    /// Failed steps; why the last failed is the index job's `reason`.
    pub(crate) failures: u64,
}

impl NetworkIndex {
    /// One indexer step, what [`db::save`] writes: the tip as `after` has
    /// it, confirmed `confirmations` below it ([`base::Tips::confirmed`]),
    /// `checked_at` `now`, and each counter what it grew by since `before`
    /// (the stats last saved). `None` while no tip is known: nothing to
    /// write.
    pub(crate) fn step(
        before: &base::Stats,
        after: &base::Stats,
        confirmations: u64,
        now: u64,
    ) -> Option<Self> {
        let tip = after.tip?;
        Some(Self {
            tip,
            confirmed: base::Tips { position: tip }.confirmed(confirmations),
            indexed: after.last.as_ref().map(|last| last.position),
            checked_at: Some(now),
            blocks: after.blocks.saturating_sub(before.blocks),
            reorgs: after.reorgs.saturating_sub(before.reorgs),
            failures: after.failures.saturating_sub(before.failures),
        })
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
        tip: i64,
        confirmed: i64,
        indexed: Option<i64>,
        checked_at: Option<i64>,
        blocks: i64,
        reorgs: i64,
        failures: i64,
    }

    impl TryFrom<Row> for (base::Network, NetworkIndex) {
        type Error = anyhow::Error;

        fn try_from(row: Row) -> anyhow::Result<Self> {
            let key = base::Network::new(base::Chain::new(&row.chain), &row.network);
            let index = NetworkIndex {
                tip: uint(row.tip)?,
                confirmed: uint(row.confirmed)?,
                indexed: row.indexed.map(uint).transpose()?,
                checked_at: row.checked_at.map(uint).transpose()?,
                blocks: uint(row.blocks)?,
                reorgs: uint(row.reorgs)?,
                failures: uint(row.failures)?,
            };
            Ok((key, index))
        }
    }

    /// Writes one step ([`NetworkIndex::step`]) to `network`'s row, the
    /// row made by the first. `tip` and `confirmed` only move forward,
    /// unless the step reorged blocks (a reorg, which may shorten the
    /// chain): a step read from a node behind the last one (a failover)
    /// keeps them. `indexed` takes a known position. `checked_at` moves
    /// only when the tip does (a node that stopped producing does not look
    /// fresh), and the counters add up.
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
            INSERT INTO network_index
                (chain, network, tip, confirmed, indexed, checked_at, blocks, reorgs, failures)
            VALUES (?8, ?9, ?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT (chain, network) DO UPDATE
            SET checked_at = IIF(
                    ?1 > tip OR (?6 > 0 AND ?1 IS NOT tip),
                    ?4,
                    checked_at
                ),
                tip = IIF(?1 > tip OR ?6 > 0, ?1, tip),
                confirmed = IIF(?2 > confirmed OR ?6 > 0, ?2, confirmed),
                indexed = COALESCE(?3, indexed),
                blocks = blocks + ?5,
                reorgs = reorgs + ?6,
                failures = failures + ?7",
        )
        .bind::<BigInt, _>(int(step.tip)?)
        .bind::<BigInt, _>(int(step.confirmed)?)
        .bind::<Nullable<BigInt>, _>(position(step.indexed)?)
        .bind::<Nullable<BigInt>, _>(position(step.checked_at)?)
        .bind::<BigInt, _>(int(step.blocks)?)
        .bind::<BigInt, _>(int(step.reorgs)?)
        .bind::<BigInt, _>(int(step.failures)?)
        .bind::<Text, _>(network.chain.as_str())
        .bind::<Text, _>(&network.name)
        .execute(connection)?;
        Ok(())
    }

    /// `network`'s confirmed position; `None` while unknown (no row: no tip
    /// yet).
    pub(crate) fn find_confirmed(
        connection: &mut SqliteConnection,
        network: &base::Network,
    ) -> QueryResult<Option<i64>> {
        network_index::table
            .filter(network_index::chain.eq(network.chain.as_str()))
            .filter(network_index::network.eq(&network.name))
            .select(network_index::confirmed)
            .first::<i64>(connection)
            .optional()
    }

    /// Every row: each network whose tip is known.
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
