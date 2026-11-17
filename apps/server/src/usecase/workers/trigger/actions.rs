//! What a trigger does once its check passes: this server's actions, each a
//! `trigger` package [`Action`] returning the jobs to start, never starting
//! one itself. `send` ([`Transfer`]) is the one there is.

use ::trigger::{
    Class, Context, Error,
    actions::{Action, Registry},
    amounts::Amount as _,
    selection::Target,
};
use anyhow::{bail, ensure};
use base::{Asset, Assets, Decimal, Future, Network};
use serde::{Deserialize, Serialize};
use tasks::{Job, Worker as _};

use crate::{
    models::trigger::Amount,
    usecase::workers::broadcast::{Broadcast, SendIntent},
};

/// Every action a trigger of this server may call.
pub(crate) fn registry() -> Registry<Job> {
    Registry::new().add(Transfer)
}

/// `send`: moves funds, one broadcast job per payment.
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

impl Action<Job> for Transfer {
    const NAME: &'static str = "send";
    type Input = SendInput;

    /// The sent asset is configured on `network` (`NotConfigured`), its
    /// wallet sets name wallets on purpose, and it is not many to many
    /// (`This` to `This`).
    fn validate(&self, send: &SendInput, network: &Network, assets: &Assets) -> Result<(), Error> {
        let asset = Asset {
            network: network.clone(),
            id: send.asset.clone(),
        };
        if assets.get(&asset).is_none() {
            return Err(Error::new(Class::NotConfigured, "asset is not configured"));
        }
        send.source.validate()?;
        send.destination.validate()?;
        send.amount.validate()?;
        if send.source.is_this() && send.destination.is_this() {
            let why = "a send from This to This is many to many";
            return Err(Error::new(Class::Refused, why));
        }
        Ok(())
    }

    /// Many sources pay one destination, `amount` evaluated per source; one
    /// source pays many destinations, `amount` per destination; one to one
    /// is per destination when the destination is `This`. A payment whose
    /// amount is zero, negative or unknown is skipped. Payments go in source
    /// then destination order, one broadcast job each.
    ///
    /// Refuses many sources and many destinations, a source in another
    /// asset than the send's, a target not fully indexed, and a source not
    /// keyed here.
    fn run<'a>(
        &'a self,
        send: &'a SendInput,
        on: &'a mut Context<'_>,
    ) -> Future<'a, anyhow::Result<Vec<Job>>> {
        Box::pin(async move {
            let Some(sources) = on.wallets(&send.source).await? else {
                bail!("a source is not fully indexed");
            };
            let Some(destinations) = on.wallets(&send.destination).await? else {
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
            // The side that is many is walked, the other is its one address;
            // one to one is walked on the side the send names as `This`.
            let per_source = match (sources.len(), destinations.len()) {
                (1, 1) => !send.destination.is_this(),
                (_, destinations) => destinations <= 1,
            };
            let (each, other) = if per_source {
                (&sources, &destinations)
            } else {
                (&destinations, &sources)
            };
            let Some(other) = other.values().next() else {
                return Ok(Vec::new());
            };
            let network = on.network().clone();
            let asset = Asset {
                network: network.clone(),
                id: send.asset.clone(),
            };
            let mut jobs = Vec::new();
            for one in each.values() {
                let mut alone = on.with(each.only(one)?);
                let amount = send.amount.atomic(&mut alone, &asset).await?;
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
                jobs.push(Broadcast::job(&SendIntent {
                    chain: network.chain.as_str().to_owned(),
                    network: network.name.clone(),
                    address: from.address.value.clone(),
                    asset: send.asset.clone(),
                    to: to.address.value.clone(),
                    amount: amount.to_string(),
                })?);
            }
            Ok(jobs)
        })
    }
}
