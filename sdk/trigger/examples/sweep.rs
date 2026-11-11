//! "Token sweep with gas funding", offline: the server's two-trigger
//! template (`apps/server/ui/src/pages/triggers/_components/forms.ts`) run
//! for one round over wallets held in memory.
//!
//! It shows the package the way the server uses it: a trigger language of
//! its own (`Filter`, `Check`, `Term`), an action of its own (`send`, here
//! recording what it would send), triggers parsed from their stored JSON, a
//! [`Round`] at one confirmed block, and the decisions it returns.
//!
//! Run with `cargo run -p trigger --example sweep`.

use std::sync::Arc;

use anyhow::{bail, ensure};
use base::{Address, Asset, AssetInfo, Assets, Chain, Decimal, Future, Network};
use futures_executor::block_on;
use serde::{Deserialize, Serialize};
use serde_json::json;
use trigger::{
    Class, Context, Enabled, Error, Round, Trigger,
    actions::{Action, Registry},
    amounts::{self, Amount as _, Arithmetic},
    conditions::{self, Condition},
    selection::{Index, Target},
    testing::{Fees, Wallets},
};

/// The token swept: Sepolia's USDC.
const TOKEN: &str = "0x1c7d4b196cb0c7b01d743fbc6116a902379c7238";

// ---- The trigger language: which building blocks, and the JSON's shape ----

/// The amount terms on offer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Term {
    Value(amounts::Value),
    Balance(amounts::Balance),
    Fee(amounts::Fee),
}

type Amount = Arithmetic<Term>;
type Compare = conditions::Compare<Amount>;

/// What a filter may test, per index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Filter {
    Compare(Compare),
}

/// What a check may test, over the kept set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Check {
    Since(conditions::Since),
    Compare(Compare),
}

impl amounts::Amount for Term {
    fn atomic<'a>(
        &'a self,
        on: &'a mut Context<'_>,
        asset: &'a Asset,
    ) -> Future<'a, anyhow::Result<Option<Decimal>>> {
        match self {
            Self::Value(one) => one.atomic(on, asset),
            Self::Balance(one) => one.atomic(on, asset),
            Self::Fee(one) => one.atomic(on, asset),
        }
    }

    fn validate(&self) -> Result<(), Error> {
        match self {
            Self::Value(one) => one.validate(),
            Self::Balance(one) => one.validate(),
            Self::Fee(one) => one.validate(),
        }
    }

    fn reads_this(&self) -> bool {
        match self {
            Self::Value(one) => one.reads_this(),
            Self::Balance(one) => one.reads_this(),
            Self::Fee(one) => one.reads_this(),
        }
    }

    fn constant(&self) -> Option<&Decimal> {
        match self {
            Self::Value(one) => one.constant(),
            Self::Balance(one) => one.constant(),
            Self::Fee(one) => one.constant(),
        }
    }
}

impl Condition for Filter {
    fn holds<'a>(&'a self, on: &'a mut Context<'_>) -> Future<'a, anyhow::Result<bool>> {
        match self {
            Self::Compare(one) => one.holds(on),
        }
    }

    fn validate(&self) -> Result<(), Error> {
        match self {
            Self::Compare(one) => one.validate(),
        }
    }

    fn reads_this(&self) -> bool {
        match self {
            Self::Compare(one) => one.reads_this(),
        }
    }
}

impl Condition for Check {
    fn holds<'a>(&'a self, on: &'a mut Context<'_>) -> Future<'a, anyhow::Result<bool>> {
        match self {
            Self::Since(one) => one.holds(on),
            Self::Compare(one) => one.holds(on),
        }
    }

    fn validate(&self) -> Result<(), Error> {
        match self {
            Self::Since(one) => one.validate(),
            Self::Compare(one) => one.validate(),
        }
    }

    fn reads_this(&self) -> bool {
        match self {
            Self::Since(one) => one.reads_this(),
            Self::Compare(one) => one.reads_this(),
        }
    }
}

type Definition = Trigger<Filter, Check>;

// ---- The action: `send`, recording what it would send ---------------------

/// One transfer `send` asks for; the server's would be a broadcast job.
#[derive(Debug)]
struct Sent {
    from: Address,
    to: Address,
    asset: String,
    amount: Decimal,
}

/// `send`'s stored input.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendInput {
    source: Target,
    asset: String,
    destination: Target,
    amount: Amount,
}

/// `send`: `amount` of `asset` from `source` to `destination`, walked on the
/// side that is many (one to one: on the side named `This`).
struct Send;

impl Action<Sent> for Send {
    const NAME: &'static str = "send";
    type Input = SendInput;

    fn validate(&self, send: &SendInput, network: &Network, assets: &Assets) -> Result<(), Error> {
        send.source.validate()?;
        send.destination.validate()?;
        send.amount.validate()?;
        if send.source.is_this() && send.destination.is_this() {
            let why = "a send from This to This is many to many";
            return Err(Error::new(Class::Refused, why));
        }
        if assets.get(&asset(network, &send.asset)).is_none() {
            return Err(Error::new(Class::NotConfigured, "asset is not configured"));
        }
        Ok(())
    }

    fn run<'a>(
        &'a self,
        send: &'a SendInput,
        on: &'a mut Context<'_>,
    ) -> Future<'a, anyhow::Result<Vec<Sent>>> {
        Box::pin(async move {
            let Some(sources) = on.wallets(&send.source).await? else {
                bail!("a source is not fully indexed");
            };
            let Some(destinations) = on.wallets(&send.destination).await? else {
                bail!("a destination is not fully indexed");
            };
            ensure!(
                sources.len() <= 1 || destinations.len() <= 1,
                "a send has many sources and many destinations"
            );
            let per_source = match (sources.len(), destinations.len()) {
                (1, 1) => !send.destination.is_this(),
                (_, many) => many <= 1,
            };
            let (each, other) = match per_source {
                true => (&sources, &destinations),
                false => (&destinations, &sources),
            };
            let Some(other) = other.values().next().cloned() else {
                return Ok(Vec::new());
            };
            let asset = asset(on.network(), &send.asset);
            let mut sent = Vec::new();
            for one in each.values() {
                let mut alone = on.with(each.only(one)?);
                let amount = send.amount.atomic(&mut alone, &asset).await?;
                let Some(amount) = amount.filter(|amount| *amount > Decimal::zero()) else {
                    continue;
                };
                let (from, to) = if per_source {
                    (one, &other)
                } else {
                    (&other, one)
                };
                ensure!(from.keyed, "source {} is not keyed here", from.address);
                let (from, to) = (from.address.clone(), to.address.clone());
                let asset = send.asset.clone();
                sent.push(Sent {
                    from,
                    to,
                    asset,
                    amount,
                });
            }
            Ok(sent)
        })
    }
}

fn asset(network: &Network, id: &str) -> Asset {
    Asset {
        network: network.clone(),
        id: id.to_owned(),
    }
}

// ---- One round ---------------------------------------------------------------

/// The template's two triggers, as stored, the gas wallet chosen.
fn template(gas: &str) -> [(&'static str, serde_json::Value); 2] {
    let chosen = json!({ "select": { "ids": [gas] } });
    let fee = |of: serde_json::Value| json!({ "fee": { "of": of, "to": chosen } });
    let this = json!({ "this": {} });
    let this_token = json!({ "this": { "asset": TOKEN } });
    let tagged = |asset: &str| json!({ "all": [{ "tag": "deposit" }, { "asset": asset }] });
    let compare = |of: serde_json::Value,
                   balance: &str,
                   operator: &str,
                   value: serde_json::Value| {
        json!({ "compare": { "of": of, "aggregate": "sum", "balance": balance, "operator": operator, "value": value } })
    };
    let pending = json!({ "balance": { "of": this, "balance": "pending" } });
    let shortfall = json!({ "max": [{ "sub": [{ "times": [fee(this_token.clone()), "1.25"] }, pending] }, { "value": "0" }] });
    let fund = json!({
        "selector": tagged("native"),
        "filter": { "all": [
            compare(this_token.clone(), "confirmed", "above", json!({ "value": "0" })),
            compare(this.clone(), "pending", "below", fee(this_token)),
        ] },
        "check": { "all": [] },
        "action": { "name": "send", "input": {
            "source": chosen, "asset": "native", "destination": this, "amount": shortfall,
        } },
    });
    let sweep = json!({
        "selector": tagged(TOKEN),
        "filter": { "all": [
            compare(this.clone(), "confirmed", "above", json!({ "value": "0" })),
            compare(json!({ "this": { "asset": "native" } }), "confirmed", "at_least", fee(this.clone())),
        ] },
        "check": { "all": [] },
        "action": { "name": "send", "input": {
            "source": this, "asset": TOKEN, "destination": chosen, "amount": { "balance": { "of": { "this": {} } } },
        } },
    });
    [("fund-gas", fund), ("sweep-token", sweep)]
}

/// The index of `asset` of `address` holding `confirmed` and `pending`
/// atomic units, keyed.
fn index(network: &Network, address: &str, asset: &str, confirmed: u64, pending: u64) -> Index {
    let mut one = Index::new(
        Address::new(network.clone(), address),
        self::asset(network, asset),
    );
    (one.confirmed, one.pending, one.keyed) = (confirmed.into(), pending.into(), true);
    one
}

fn main() -> anyhow::Result<()> {
    let network = Network::new(Chain::new("ethereum"), "sepolia");
    let ether = AssetInfo::token(Asset::native(network.clone()), 18);
    let assets: Assets = [ether, AssetInfo::token(asset(&network, TOKEN), 6)]
        .into_iter()
        .collect();

    // The store at the confirmed block: a gas wallet and three deposits.
    let gwei = 1_000_000_000;
    let mut wallets = Wallets::new();
    wallets
        .add(
            index(
                &network,
                "0x9a5",
                "native",
                1_000_000 * gwei,
                1_000_000 * gwei,
            ),
            &["gas"],
        )
        // Holds the token, no gas: funded.
        .add(index(&network, "0xd01", "native", 0, 0), &["deposit"])
        .add(
            index(&network, "0xd01", TOKEN, 50_000_000, 50_000_000),
            &["deposit"],
        )
        // Holds the token and its gas: swept.
        .add(
            index(&network, "0xd02", "native", 30_000 * gwei, 30_000 * gwei),
            &["deposit"],
        )
        .add(
            index(&network, "0xd02", TOKEN, 20_000_000, 20_000_000),
            &["deposit"],
        )
        // Holds nothing: neither.
        .add(index(&network, "0xd03", "native", 0, 0), &["deposit"]);

    // Its triggers, parsed from their stored JSON and checked as the API would.
    let actions = Registry::new().add(Send);
    let gas = "ethereum.sepolia.0x9a5.native";
    let mut triggers = Vec::new();
    for (id, stored) in template(gas) {
        let trigger: Definition = serde_json::from_value(stored)?;
        trigger.validate(&actions, &network, &assets)?;
        triggers.push(Enabled {
            id: id.to_owned(),
            trigger,
            last_run: None,
        });
    }

    // One round at confirmed block 7_000_000, each spend priced 21,000 gwei.
    let fees = Arc::new(Fees::new(network.clone(), (21_000 * gwei).into()));
    let round = Round::new(
        &actions,
        fees.clone(),
        &assets,
        network.clone(),
        7_000_000,
        1_700_000_000_000,
    );
    let decided = block_on(round.run(&mut wallets, triggers));

    for decision in &decided {
        let last = &decision.last;
        println!(
            "{}: at block {}, selected {}, kept {}, checked {}, jobs {}, refused {:?}",
            decision.trigger,
            last.confirmed,
            last.stats.selected,
            last.stats.kept,
            last.stats.checked,
            last.stats.jobs,
            last.refused,
        );
        for sent in &decision.effects {
            println!(
                "  send {} of {} from {} to {}",
                sent.amount, sent.asset, sent.from, sent.to
            );
        }
    }
    println!(
        "fee estimates asked: {}; store reads: {}",
        fees.asked().len(),
        wallets.reads()
    );
    Ok(())
}
