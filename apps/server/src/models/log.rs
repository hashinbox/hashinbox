//! `log`: the events the server keeps, one row each, as its `tracing`
//! layer read them: written in batches (`usecase::logs::DbLayer`), pruned
//! by the prune job by level. A network's entry (a `network` field,
//! `chain/name`) names its network, a job's (a `job_id` field, the
//! executor's span) its job, a wallet's (a `wallet_id` field, or a
//! `wallet` address of its network) its wallet, a transaction's (a
//! `transaction_id` field, or a `txid` of its network) its transaction, a
//! trigger's (a `trigger_id` or `trigger` field) its trigger; any other
//! names none.

use std::collections::BTreeMap;

use base::{Address, Chain, Network};
use diesel::{prelude::*, sqlite::Sqlite};
use serde::Serialize;

use super::{Json, schema::log, transaction::TransactionId, wallet::WalletId};
use crate::db::{Keyed, Page};

/// One kept event.
#[derive(
    Clone, Debug, PartialEq, Eq, Queryable, Selectable, Insertable, Serialize, utoipa::ToSchema,
)]
#[diesel(table_name = log, check_for_backend(Sqlite))]
#[schema(as = LogEntry)]
pub(crate) struct Log {
    /// Its id, newest highest; 0 until stored, so 0 on the live stream
    /// (`/v1/logs/stream`).
    #[diesel(skip_insertion)]
    pub(crate) id: i64,
    /// When it happened, Unix ms.
    pub(crate) at: i64,
    /// `error`, `warn`, `info`, `debug` or `trace`.
    pub(crate) level: String,
    /// The event: `rpc.http`, `index.step`, `send.broadcast`, …
    pub(crate) target: String,
    pub(crate) message: String,
    /// Its fields and its spans' (`network`, `node`, …), as text.
    #[schema(value_type = BTreeMap<String, String>)]
    pub(crate) fields: Json<BTreeMap<String, String>>,
    /// Its network's chain; always present, null: no network's.
    #[schema(required)]
    pub(crate) chain: Option<String>,
    /// Its network's name; always present, null: no network's.
    #[schema(required)]
    pub(crate) network: Option<String>,
    /// The job it ran in; always present, null: none.
    #[schema(required)]
    pub(crate) job_id: Option<i64>,
    /// The wallet it concerns (`{chain}.{network}.{address}`); always
    /// present, null: none.
    #[schema(required)]
    pub(crate) wallet_id: Option<String>,
    /// The transaction it concerns (`{chain}.{network}.{txid}`); always
    /// present, null: none.
    #[schema(required)]
    pub(crate) transaction_id: Option<String>,
    /// The trigger it concerns; always present, null: none.
    #[schema(required)]
    pub(crate) trigger_id: Option<String>,
}

/// Which entries a page reads; each filter unset reads all.
#[derive(Clone, Debug, Default)]
pub(crate) struct LogFilter {
    /// Of this network only.
    pub(crate) network: Option<base::Network>,
    /// Of this job only.
    pub(crate) job_id: Option<i64>,
    /// Of this wallet only.
    pub(crate) wallet_id: Option<String>,
    /// Of this transaction only.
    pub(crate) transaction_id: Option<String>,
    /// Of this trigger only.
    pub(crate) trigger_id: Option<String>,
    /// At this level or more severe; exactly it when `only`.
    pub(crate) level: Option<tracing::Level>,
    /// `level` alone, none more severe.
    pub(crate) only: bool,
    /// Of these targets (events) only; empty: of every one.
    pub(crate) targets: Vec<String>,
    /// From before this Unix ms only.
    pub(crate) before: Option<i64>,
    /// From this Unix ms on only; a page then reads oldest first.
    pub(crate) after: Option<i64>,
}

impl Log {
    /// Event `target` at `level`, read at `at` (Unix ms), with `message`
    /// and `fields` (its own and its spans'): its network from a `network`
    /// field (`chain/name`), its job from a `job_id` one, its wallet from a
    /// `wallet_id` one or a `wallet` address of its network, its
    /// transaction from a `transaction_id` one or a `txid` of its network,
    /// its trigger from a `trigger_id` or a `trigger` one.
    pub(crate) fn new(
        at: i64,
        level: tracing::Level,
        target: String,
        message: String,
        fields: BTreeMap<String, String>,
    ) -> Self {
        let named = fields
            .get("network")
            .and_then(|label| label.split_once('/'))
            .map(|(chain, name)| Network::new(Chain::new(chain), name));
        let job_id = fields.get("job_id").and_then(|id| id.parse().ok());
        let wallet = named.as_ref().zip(fields.get("wallet"));
        let wallet = wallet.and_then(|(network, wallet)| {
            WalletId::of(&Address::new(network.clone(), wallet)).ok()
        });
        let wallet_id = fields.get("wallet_id").cloned();
        let transaction = named.as_ref().zip(fields.get("txid"));
        let transaction =
            transaction.and_then(|(network, txid)| TransactionId::new(network, txid).ok());
        let transaction_id = fields.get("transaction_id").cloned();
        let trigger_id = fields.get("trigger_id").or_else(|| fields.get("trigger"));
        let trigger_id = trigger_id.cloned();
        Self {
            id: 0,
            at,
            level: level.as_str().to_ascii_lowercase(),
            target,
            message,
            chain: named.as_ref().map(|network| network.chain.to_string()),
            network: named.map(|network| network.name),
            fields: Json(fields),
            job_id,
            wallet_id: wallet_id.or_else(|| wallet.map(String::from)),
            transaction_id: transaction_id.or_else(|| transaction.map(String::from)),
            trigger_id,
        }
    }
}

/// Every level, most severe first.
const LEVELS: [tracing::Level; 5] = [
    tracing::Level::ERROR,
    tracing::Level::WARN,
    tracing::Level::INFO,
    tracing::Level::DEBUG,
    tracing::Level::TRACE,
];

impl LogFilter {
    /// Whether it reads `log`, as [`db::find_page`] would.
    pub(crate) fn matches(&self, log: &Log) -> bool {
        let network = self.network.as_ref().is_none_or(|network| {
            log.chain.as_deref() == Some(network.chain.to_string().as_str())
                && log.network.as_deref() == Some(network.name.as_str())
        });
        let level = log.level.parse::<tracing::Level>().ok();
        let level = level.is_some_and(|level| self.levels().contains(&level));
        network
            && level
            && self.job_id.is_none_or(|id| log.job_id == Some(id))
            && same(self.wallet_id.as_ref(), log.wallet_id.as_ref())
            && same(self.transaction_id.as_ref(), log.transaction_id.as_ref())
            && same(self.trigger_id.as_ref(), log.trigger_id.as_ref())
            && (self.targets.is_empty() || self.targets.contains(&log.target))
            && self.before.is_none_or(|before| log.at < before)
            && self.after.is_none_or(|after| log.at >= after)
    }

    /// The levels it reads, by name; every one when unset.
    fn level_names(&self) -> Vec<String> {
        let names = self.levels().into_iter();
        names
            .map(|level| level.as_str().to_ascii_lowercase())
            .collect()
    }

    /// The levels it reads: `level` and the more severe, or `level` alone
    /// (`only`); every one when unset.
    fn levels(&self) -> Vec<tracing::Level> {
        let Some(wanted) = self.level else {
            return LEVELS.to_vec();
        };
        let read = LEVELS.into_iter();
        read.filter(|level| (!self.only && *level <= wanted) || *level == wanted)
            .collect()
    }
}

/// Whether `held` is `wanted`, or nothing is wanted.
fn same(wanted: Option<&String>, held: Option<&String>) -> bool {
    wanted.is_none_or(|wanted| held == Some(wanted))
}

/// An entry's key is its id.
impl Keyed for Log {
    fn key(&self) -> String {
        self.id.to_string()
    }
}

/// The `log` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    /// Stores `logs`, each its own row.
    ///
    /// # Errors
    ///
    /// When a write fails.
    pub(crate) fn insert_all(connection: &mut SqliteConnection, logs: &[Log]) -> QueryResult<()> {
        diesel::insert_into(log::table)
            .values(logs)
            .execute(connection)
            .map(drop)
    }

    /// Every event (`target`) an entry is kept of, each once, in order.
    pub(crate) fn find_targets(connection: &mut SqliteConnection) -> QueryResult<Vec<String>> {
        log::table
            .select(log::target)
            .distinct()
            .order(log::target.asc())
            .load(connection)
    }

    /// A page of the entries `filter` reads: newest first, after the
    /// cursor's id; with `filter.after`, oldest first, from its moment and
    /// past the cursor's id.
    pub(crate) fn find_page(
        connection: &mut SqliteConnection,
        filter: &LogFilter,
        page: Page<i64>,
    ) -> QueryResult<Vec<Log>> {
        let mut query = log::table.into_boxed();
        if let Some(network) = &filter.network {
            query = query
                .filter(log::chain.eq(network.chain.to_string()))
                .filter(log::network.eq(network.name.clone()));
        }
        if let Some(job_id) = filter.job_id {
            query = query.filter(log::job_id.eq(job_id));
        }
        if let Some(wallet_id) = &filter.wallet_id {
            query = query.filter(log::wallet_id.eq(wallet_id.clone()));
        }
        if let Some(transaction_id) = &filter.transaction_id {
            query = query.filter(log::transaction_id.eq(transaction_id.clone()));
        }
        if let Some(trigger_id) = &filter.trigger_id {
            query = query.filter(log::trigger_id.eq(trigger_id.clone()));
        }
        if filter.level.is_some() {
            query = query.filter(log::level.eq_any(filter.level_names()));
        }
        if !filter.targets.is_empty() {
            query = query.filter(log::target.eq_any(filter.targets.clone()));
        }
        if let Some(before) = filter.before {
            query = query.filter(log::at.lt(before));
        }
        let Some(after) = filter.after else {
            if let Some(cursor) = page.after {
                query = query.filter(log::id.lt(cursor));
            }
            return query
                .order(log::id.desc())
                .limit(page.limit)
                .select(Log::as_select())
                .load(connection);
        };
        query = query.filter(log::at.ge(after));
        if let Some(cursor) = page.after {
            query = query.filter(log::id.gt(cursor));
        }
        query
            .order(log::id.asc())
            .limit(page.limit)
            .select(Log::as_select())
            .load(connection)
    }

    /// Deletes the `debug` and `trace` entries from before `routine_before`
    /// (Unix ms), and every entry from before `before`; how many.
    pub(crate) fn prune(
        connection: &mut SqliteConnection,
        routine_before: i64,
        before: i64,
    ) -> QueryResult<usize> {
        let routine = log::level
            .eq_any(["debug", "trace"])
            .and(log::at.lt(routine_before));
        diesel::delete(log::table.filter(routine.or(log::at.lt(before)))).execute(connection)
    }

    /// Deletes each job's entries but its newest `keep`; how many. Per job
    /// one seek for its `keep`th newest id, then one range of `log_job`
    /// below it.
    pub(crate) fn delete_beyond_per_job(
        connection: &mut SqliteConnection,
        keep: i64,
    ) -> QueryResult<usize> {
        diesel::sql_query(
            "WITH cut AS MATERIALIZED (
                SELECT j.job_id, (
                    SELECT c.id FROM log c WHERE c.job_id = j.job_id
                    ORDER BY c.id DESC LIMIT 1 OFFSET ?
                ) AS below
                FROM (SELECT DISTINCT job_id FROM log WHERE job_id IS NOT NULL) j
            )
            DELETE FROM log WHERE id IN (
                SELECT l.id FROM cut CROSS JOIN log l
                ON l.job_id = cut.job_id AND l.id < cut.below
            )",
        )
        .bind::<diesel::sql_types::BigInt, _>(keep.saturating_sub(1))
        .execute(connection)
    }

    /// Deletes the entries of no job but each network's newest `keep`, and
    /// no network's newest `keep`; how many. Per network one seek for its
    /// `keep`th newest such id, then one range of `log_network` below it.
    pub(crate) fn delete_jobless_beyond_per_network(
        connection: &mut SqliteConnection,
        keep: i64,
    ) -> QueryResult<usize> {
        diesel::sql_query(
            "WITH cut AS MATERIALIZED (
                SELECT n.chain, n.network, (
                    SELECT c.id FROM log c
                    WHERE c.chain IS n.chain AND c.network IS n.network AND c.job_id IS NULL
                    ORDER BY c.id DESC LIMIT 1 OFFSET ?
                ) AS below
                FROM (SELECT DISTINCT chain, network FROM log) n
            )
            DELETE FROM log WHERE id IN (
                SELECT l.id FROM cut CROSS JOIN log l
                ON l.chain IS cut.chain AND l.network IS cut.network AND l.id < cut.below
                WHERE l.job_id IS NULL
            )",
        )
        .bind::<diesel::sql_types::BigInt, _>(keep.saturating_sub(1))
        .execute(connection)
    }
}
