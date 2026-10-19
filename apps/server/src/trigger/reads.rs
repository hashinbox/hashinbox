//! [`Reads`]: everything one evaluation of one trigger reads, each read made
//! at most once, so the check, filter and action see the same figures. Runs
//! on a blocking thread (it holds a SQLite connection); a fee is awaited on
//! `runtime`.

use std::{collections::BTreeMap, sync::Arc};

use base::{Decimal, FeeEstimator};
use diesel::SqliteConnection;

use super::{Decimals, Target, Targets};
use crate::models::{TriggerRun, wallet_index_balance::WalletIndexBalances};

/// What a check needs from the trigger's latest run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LastRun {
    /// When it started (Unix ms).
    pub(crate) started_at: i64,
    /// Every send of it is confirmed or definitively failed.
    pub(crate) settled: bool,
}

/// The reads already made.
#[derive(Default)]
pub(crate) struct Seen {
    /// Native atomic fee per set of wallet index ids.
    fees: BTreeMap<Vec<String>, Decimal>,
    /// The latest run.
    run: Option<Option<LastRun>>,
}

/// One evaluation's sources and the reads made from them.
pub(crate) struct Reads<'a> {
    pub(crate) connection: &'a mut SqliteConnection,
    /// Unix ms.
    pub(crate) now: i64,
    /// The network's fee estimator (`WalletFactory::fees()`).
    pub(crate) fees: Arc<dyn FeeEstimator>,
    /// Where a fee estimate is awaited.
    pub(crate) runtime: tokio::runtime::Handle,
    /// Decimals per asset id of the network.
    pub(crate) decimals: Decimals,
    /// Targets resolved so far.
    pub(crate) targets: Targets,
    /// Reads the trigger's latest run (until the run table exists).
    pub(crate) last_run: Box<dyn FnMut() -> anyhow::Result<Option<LastRun>> + 'a>,
    pub(crate) seen: Seen,
}

impl Reads<'_> {
    /// What `target` names with `this` in context (see [`Targets::find`]).
    pub(crate) fn target(
        &mut self,
        target: &Target,
        this: &WalletIndexBalances,
    ) -> anyhow::Result<Option<WalletIndexBalances>> {
        self.targets.find(self.connection, this, target)
    }

    /// The native atomic fee of sending everything `indexes` hold to one
    /// address (empty: 0).
    pub(crate) fn fee(&mut self, indexes: &WalletIndexBalances) -> anyhow::Result<Decimal> {
        if indexes.is_empty() {
            return Ok(Decimal::zero());
        }
        let key = indexes.ids();
        if let Some(fee) = self.seen.fees.get(&key) {
            return Ok(fee.clone());
        }
        let fee = self
            .runtime
            .block_on(self.fees.estimate(&indexes.sources()))?;
        self.seen.fees.insert(key, fee.clone());
        Ok(fee)
    }

    /// The trigger's latest run.
    pub(crate) fn run(&mut self) -> anyhow::Result<Option<LastRun>> {
        if let Some(run) = self.seen.run {
            return Ok(run);
        }
        let run = (self.last_run)()?;
        self.seen.run = Some(run);
        Ok(run)
    }
}

impl From<TriggerRun> for LastRun {
    fn from(run: TriggerRun) -> Self {
        Self {
            started_at: run.started_at,
            settled: run.settled_at.is_some(),
        }
    }
}
