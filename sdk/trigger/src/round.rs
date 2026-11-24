//! One round: every enabled trigger of a network decides at one confirmed
//! block, over one read of its [`Store`]. A refused trigger never stops the others, and
//! every trigger gets its [`LastRound`].

use std::sync::Arc;

use base::{Assets, FeeEstimator, Network};
use serde::{Deserialize, Serialize};
use tracing::Instrument as _;

use crate::{
    actions::Registry,
    conditions::Condition,
    store::{CachedStore, Store},
    trigger::{Context, Trigger},
};

/// An enabled trigger with its id and its latest run: what a round takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Enabled<F, C> {
    /// Its id.
    pub id: String,
    /// Its definition.
    pub trigger: Trigger<F, C>,
    /// When its latest run started, Unix ms; `None`: never.
    pub last_run: Option<u64>,
}

/// What one trigger decided this round: its last round and its effects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision<E> {
    /// The trigger's id.
    pub trigger: String,
    /// What it saw.
    pub last: LastRound,
    /// What its action asks to be carried out; empty: nothing.
    pub effects: Vec<E>,
}

/// What one round saw of a trigger: how many wallet indexes its selector
/// picked, how many of them its filter kept, whether its check held over
/// those, and how many effects its action returned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "openapi", schema(as = TriggerStats))]
pub struct Stats {
    /// Wallet indexes its selector picked.
    pub selected: usize,
    /// How many of them its filter kept (all of them without a filter).
    pub kept: usize,
    /// Whether its check held over the kept ones.
    pub checked: bool,
    /// Effects (jobs) its action returned.
    pub jobs: usize,
}

/// A trigger's last round, whether it acted or not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct LastRound {
    /// When it decided, Unix ms.
    pub at: u64,
    /// The confirmed block position it decided at.
    pub confirmed: u64,
    /// What it saw.
    #[serde(flatten)]
    pub stats: Stats,
    /// Why it was refused (it then acted on nothing); always present.
    #[cfg_attr(feature = "openapi", schema(required))]
    pub refused: Option<String>,
}

/// Every enabled trigger of a network at one confirmed block.
pub struct Round<'a, E> {
    actions: &'a Registry<E>,
    fees: Arc<dyn FeeEstimator>,
    assets: &'a Assets,
    network: Network,
    confirmed: u64,
    now: u64,
}

impl<'a, E: 'static> Round<'a, E> {
    /// A round of `network` at its `confirmed` position, at `now` (Unix ms),
    /// running `actions`, pricing with `fees`.
    #[must_use]
    pub fn new(
        actions: &'a Registry<E>,
        fees: Arc<dyn FeeEstimator>,
        assets: &'a Assets,
        network: Network,
        confirmed: u64,
        now: u64,
    ) -> Self {
        Self {
            actions,
            fees,
            assets,
            network,
            confirmed,
            now,
        }
    }

    /// Every trigger in the order given, each read of `store` answered once
    /// per round; each decides in a `trigger` span naming it (`trigger_id`).
    pub async fn run<F: Condition, C: Condition>(
        self,
        store: &mut dyn Store,
        triggers: Vec<Enabled<F, C>>,
    ) -> Vec<Decision<E>> {
        let mut store = CachedStore::new(store);
        let mut decided = Vec::with_capacity(triggers.len());
        for enabled in triggers {
            let span = tracing::info_span!("trigger", trigger_id = %enabled.id);
            decided.push(self.decide(&mut store, enabled).instrument(span).await);
        }
        decided
    }

    /// What `enabled` decides; a refusal is its `refused`, with the stats
    /// counted before it.
    async fn decide<F: Condition, C: Condition>(
        &self,
        store: &mut dyn Store,
        enabled: Enabled<F, C>,
    ) -> Decision<E> {
        let fees = &*self.fees;
        let (now, last_run) = (self.now, enabled.last_run);
        let mut on = Context::new(store, fees, self.assets, &self.network, now, last_run);
        let mut stats = Stats::default();
        let decided = enabled.trigger.decide(self.actions, &mut on, &mut stats);
        let (effects, refused) = match decided.await {
            Ok(effects) => (effects, None),
            Err(error) => (Vec::new(), Some(format!("{error:#}"))),
        };
        stats.jobs = effects.len();
        let last = LastRound {
            at: now,
            confirmed: self.confirmed,
            stats,
            refused,
        };
        Decision {
            trigger: enabled.id,
            last,
            effects,
        }
    }
}
