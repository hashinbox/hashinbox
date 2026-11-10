//! What a trigger does once its check passes: a named [`Action`] run with
//! an [`Invocation`] (the trigger, what it fired on) and its own static
//! input. Effects go through the invocation; starting jobs is the usual one,
//! never an obligation.

use std::collections::BTreeMap;

use anyhow::{Context as _, bail, ensure};
use base::{Address, Decimal, Network};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tasks::{Job, Worker as _};

use crate::{
    models::{
        trigger::{Amount, Target},
        wallet_index::Selection,
    },
    usecase::{
        trigger::Trigger,
        workers::broadcast::{Broadcast, Payment, SendIntent},
    },
};

/// What an action sees, and what it asks the run to do.
pub(crate) struct Invocation<'a, 'c> {
    /// The trigger's network.
    pub(crate) network: &'a Network,
    /// The wallet indexes it fired on (`This`), with balances.
    pub(crate) this: &'a Selection,
    /// The trigger deciding: amounts and targets, read at the decided block.
    pub(crate) trigger: &'a mut Trigger<'c>,
    started: Vec<Job>,
}

impl<'a, 'c> Invocation<'a, 'c> {
    pub(crate) const fn new(
        network: &'a Network,
        this: &'a Selection,
        trigger: &'a mut Trigger<'c>,
    ) -> Self {
        Self {
            network,
            this,
            trigger,
            started: Vec::new(),
        }
    }

    /// Asks the run to start `job`.
    pub(crate) fn start(&mut self, job: Job) {
        self.started.push(job);
    }

    /// What the action asked to start; empty: nothing to do.
    pub(crate) fn started(self) -> Vec<Job> {
        self.started
    }
}

/// One kind of action. Its input is its static settings, stored on the
/// trigger; it decodes them itself.
pub(crate) trait Action: Send + Sync {
    /// Its name, as stored on the trigger.
    fn name(&self) -> &'static str;

    /// Decides what to do now; effects only through `invocation`.
    ///
    /// # Errors
    ///
    /// When `input` does not decode, a read fails, or it refuses.
    fn run(&self, invocation: &mut Invocation<'_, '_>, input: &Value) -> anyhow::Result<()>;
}

/// Every action by name; built by the trigger worker.
#[derive(Default)]
pub(crate) struct Actions(BTreeMap<&'static str, Box<dyn Action>>);

impl Actions {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Adds `action` under its name.
    #[must_use]
    pub(crate) fn add(mut self, action: impl Action + 'static) -> Self {
        self.0.insert(action.name(), Box::new(action));
        self
    }

    /// Runs the action `name` with `input`.
    ///
    /// # Errors
    ///
    /// An unknown name, or the action's error.
    pub(crate) fn run(
        &self,
        name: &str,
        input: &Value,
        invocation: &mut Invocation<'_, '_>,
    ) -> anyhow::Result<()> {
        let action = self
            .0
            .get(name)
            .with_context(|| format!("unknown action {name}"))?;
        action.run(invocation, input)
    }
}

/// `send`: moves funds, one broadcast job per sender.
pub(crate) struct Transfer;

/// What `send` moves: `amount` of `asset` from the `source` wallet indexes
/// to the `destination` addresses. At most one side may be many: many
/// sources pay one destination (`amount` per source, `This` being it), or
/// one source pays many destinations (`amount` per destination, `This`
/// being it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendInput {
    /// Where the funds come from; every source must be keyed here.
    pub(crate) source: Target,
    /// The asset id sent; every source is read in it.
    pub(crate) asset: String,
    /// Where the funds go: the addresses of these wallet indexes.
    pub(crate) destination: Target,
    /// How much, per send, in `asset`.
    pub(crate) amount: Amount,
}

impl SendInput {
    /// Whether it may be kept: its wallet sets name wallets on purpose, and
    /// it is not many to many (`This` to `This`).
    ///
    /// # Errors
    ///
    /// The first rule it breaks.
    pub(crate) fn validate(&self) -> Result<(), crate::models::trigger::Invalid> {
        use crate::models::trigger::Invalid;
        self.source.validate()?;
        self.destination.validate()?;
        self.amount.validate()?;
        if matches!(
            (&self.source, &self.destination),
            (Target::This(_), Target::This(_))
        ) {
            return Err(Invalid::new("a send from This to This is many to many"));
        }
        Ok(())
    }
}

impl Action for Transfer {
    fn name(&self) -> &'static str {
        "send"
    }

    /// Many sources pay one destination, `amount` evaluated per source; one
    /// source pays many destinations, `amount` per destination; one to one
    /// is per destination when the destination is `This`. A transfer whose
    /// amount is zero, negative or unknown is skipped. Transfers go in
    /// source then destination order, one broadcast job per sender.
    ///
    /// Refuses many sources and many destinations, a source in another
    /// asset than the send's, a target not fully indexed, and a source not
    /// keyed here.
    fn run(&self, invocation: &mut Invocation<'_, '_>, input: &Value) -> anyhow::Result<()> {
        let send = SendInput::deserialize(input).context("send input")?;
        let (trigger, this) = (&mut *invocation.trigger, invocation.this);
        let Some(sources) = trigger.target(&send.source, this)? else {
            bail!("a source is not fully indexed");
        };
        let Some(destinations) = trigger.target(&send.destination, this)? else {
            bail!("a destination is not fully indexed");
        };
        ensure!(
            sources.values().all(|one| one.asset.id == send.asset),
            "a source is not in the sent asset"
        );
        ensure!(
            sources.len() <= 1 || destinations.len() <= 1,
            "a send has many sources and many destinations"
        );
        // The side that is many is walked, the other is its one address; one
        // to one is walked on the side the send names as `This`.
        let per_source = match (sources.len(), destinations.len()) {
            (1, 1) => !matches!(send.destination, Target::This(_)),
            (_, destinations) => destinations <= 1,
        };
        let (each, other) = if per_source {
            (&sources, &destinations)
        } else {
            (&destinations, &sources)
        };
        let Some(other) = other.values().next() else {
            return Ok(());
        };
        let mut by_sender: BTreeMap<Address, Vec<Payment>> = BTreeMap::new();
        for one in each.values() {
            let alone = each.only(one)?;
            let amount = trigger.amount(&send.amount, &alone, &send.asset)?;
            let Some(amount) = amount.filter(|amount| *amount > Decimal::zero()) else {
                continue;
            };
            let (from, to) = if per_source {
                (one, other)
            } else {
                (other, one)
            };
            ensure!(
                from.keyed,
                "source {} is not keyed here",
                from.address.value
            );
            by_sender
                .entry(from.address.clone())
                .or_default()
                .push(Payment {
                    to: to.address.value.clone(),
                    amount: amount.to_string(),
                });
        }
        for (from, transfers) in by_sender {
            let intent = SendIntent {
                chain: invocation.network.chain.as_str().to_owned(),
                network: invocation.network.name.clone(),
                address: from.value,
                asset: send.asset.clone(),
                transfers,
            };
            invocation.start(Broadcast::job(&intent)?);
        }
        Ok(())
    }
}
