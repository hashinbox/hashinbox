//! What every end-to-end test shares: a trigger language as the server
//! writes it, a `send` action that records what it would send, the UI's
//! templates as stored JSON, and a [`World`] of wallets on Sepolia.

#![allow(dead_code, reason = "each test file uses part of it")]

use std::sync::Arc;

use anyhow::{bail, ensure};
use base::{Address, Asset, AssetInfo, Assets, Chain, Decimal, Future, Network};
use futures_executor::block_on;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use trigger::{
    Class, Context, Decision, Enabled, Error, Round, Store as _, Trigger,
    actions::{Action, Registry},
    amounts::{self, Amount as _, Arithmetic},
    conditions::{self, Condition, Logic},
    selection::{Index, Selection, Selector, Target},
    testing::{Fees, Wallets},
};

// ---- The language ---------------------------------------------------------

/// The amount terms on offer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Term {
    Value(amounts::Value),
    Balance(amounts::Balance),
    Fee(amounts::Fee),
}

pub type Amount = Arithmetic<Term>;
pub type Compare = conditions::Compare<Amount>;

/// What a filter may test, per index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Filter {
    Compare(Compare),
}

/// What a check may test, over the kept set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    Since(conditions::Since),
    Compare(Compare),
}

pub type Definition = Trigger<Filter, Check>;

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

// ---- The action: `send`, recording ----------------------------------------

/// One transfer `send` asks for: addresses by their text, atomic units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    pub from: String,
    pub to: String,
    pub asset: String,
    pub amount: Decimal,
}

/// `amount` atomic units of `asset` from `from` to `to`.
pub fn sent(from: &str, to: &str, asset: &str, amount: u64) -> Sent {
    let (from, to, asset) = (from.to_owned(), to.to_owned(), asset.to_owned());
    Sent {
        from,
        to,
        asset,
        amount: amount.into(),
    }
}

/// `send`'s stored input.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendInput {
    source: Target,
    asset: String,
    destination: Target,
    amount: Amount,
}

/// `send`, as the server's: `amount` of `asset` from `source` to
/// `destination`, walked on the side that is many (one to one: on the side
/// named `This`); a transfer of 0 or less, or unknown, is skipped.
pub struct Send;

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
                sources.values().all(|one| one.asset.id == send.asset),
                "a source is not in the sent asset"
            );
            ensure!(
                sources.len() <= 1 || destinations.len() <= 1,
                "a send has many sources and many destinations"
            );
            let per_source = match (sources.len(), destinations.len()) {
                (1, 1) => !send.destination.is_this(),
                (_, many) => many <= 1,
            };
            let (each, other) = if per_source {
                (&sources, &destinations)
            } else {
                (&destinations, &sources)
            };
            let Some(other) = other.values().next().cloned() else {
                return Ok(Vec::new());
            };
            let asset = asset(on.network(), &send.asset);
            let mut all = Vec::new();
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
                all.push(Sent {
                    from: from.address.value.clone(),
                    to: to.address.value.clone(),
                    asset: send.asset.clone(),
                    amount,
                });
            }
            Ok(all)
        })
    }
}

fn asset(network: &Network, id: &str) -> Asset {
    Asset {
        network: network.clone(),
        id: id.to_owned(),
    }
}

// ---- The UI's templates, as stored ----------------------------------------

/// The time every round decides at, Unix ms.
pub const NOW: u64 = 1_700_000_000_000;
/// The confirmed block every round decides at.
pub const CONFIRMED: u64 = 7_000_000;
/// The price of one spend, native atomic units.
pub const FEE: u64 = 10;
/// The token: 2 decimals. Native has 3.
pub const TOKEN: &str = "usdc";
/// The wallet chosen in the form: the recipient, or the one paying gas.
pub const GAS: &str = "0x9a5";

/// `{"select": {"ids": [GAS's native index]}}`.
pub fn chosen() -> Value {
    json!({ "select": { "ids": [id(GAS, "native")] } })
}

/// The fee of sending everything `of` holds to the chosen wallet.
pub fn fee_of(of: Value) -> Value {
    json!({ "fee": { "of": of, "to": chosen() } })
}

fn fee() -> Value {
    fee_of(json!({ "this": {} }))
}

fn all_less_fee() -> Value {
    json!({ "sub": [{ "balance": { "of": { "this": {} } } }, fee()] })
}

fn tagged(tag: &str, asset: &str) -> Value {
    json!({ "all": [{ "tag": tag }, { "asset": asset }] })
}

fn sum(of: Value, operator: &str, value: Value) -> Value {
    json!({ "compare": { "of": of, "aggregate": "sum", "operator": operator, "value": value } })
}

fn send(source: Value, asset: &str, destination: Value, amount: Value) -> Value {
    json!({ "name": "send", "input": {
        "source": source, "asset": asset, "destination": destination, "amount": amount,
    } })
}

/// "Native forward": each deposit holding more than twice its fee sends
/// all of it less the fee.
pub fn forward() -> Value {
    json!({
        "selector": tagged("deposit", "native"),
        "filter": sum(json!({ "this": {} }), "above", json!({ "add": [fee(), fee()] })),
        "check": { "all": [] },
        "action": send(json!({ "this": {} }), "native", chosen(), all_less_fee()),
    })
}

/// "Consolidation over a threshold": once the deposits worth spending hold
/// at least 1 together, each sends all of it less its fee.
pub fn consolidate() -> Value {
    json!({
        "selector": tagged("deposit", "native"),
        "filter": sum(json!({ "this": {} }), "above", fee()),
        "check": sum(json!({ "this": {} }), "at_least", json!({ "value": "1" })),
        "action": send(json!({ "this": {} }), "native", chosen(), all_less_fee()),
    })
}

/// "Token sweep with gas funding", `fund-gas`: a deposit holding the token
/// but less pending native than the token's fee gets up to 1.25 × the fee.
pub fn fund_gas() -> Value {
    let this_token = json!({ "this": { "asset": TOKEN } });
    let pending = json!({ "balance": { "of": { "this": {} }, "balance": "pending" } });
    let headroom = json!({ "times": [fee_of(this_token.clone()), "1.25"] });
    let shortfall = json!({ "max": [{ "sub": [headroom, pending] }, { "value": "0" }] });
    let below = json!({ "compare": {
        "of": { "this": {} }, "aggregate": "sum", "balance": "pending",
        "operator": "below", "value": fee_of(this_token.clone()),
    } });
    json!({
        "selector": tagged("deposit", "native"),
        "filter": { "all": [sum(this_token, "above", json!({ "value": "0" })), below] },
        "check": { "all": [] },
        "action": send(chosen(), "native", json!({ "this": {} }), shortfall),
    })
}

/// "Token sweep with gas funding", `sweep-token`: a deposit holding the
/// token and the native for its fee sends all the token.
pub fn sweep_token() -> Value {
    let balance = json!({ "balance": { "of": { "this": {} } } });
    json!({
        "selector": tagged("deposit", TOKEN),
        "filter": { "all": [
            sum(json!({ "this": {} }), "above", json!({ "value": "0" })),
            sum(json!({ "this": { "asset": "native" } }), "at_least", fee()),
        ] },
        "check": { "all": [] },
        "action": send(json!({ "this": {} }), TOKEN, chosen(), balance),
    })
}

/// "Scheduled transfer": the treasury sends 0.1 at most once a day.
pub fn scheduled() -> Value {
    json!({
        "selector": tagged("treasury", "native"),
        "check": { "since": { "seconds": 86_400 } },
        "action": send(json!({ "this": {} }), "native", chosen(), json!({ "value": "0.1" })),
    })
}

// ---- The world --------------------------------------------------------------

/// The id of `asset` of `address` on Sepolia.
pub fn id(address: &str, asset: &str) -> String {
    format!("ethereum.sepolia.{address}.{asset}")
}

/// `json` as a `T`.
pub fn parse<T: DeserializeOwned>(json: Value) -> T {
    serde_json::from_value(json).expect("it decodes")
}

/// Enabled trigger `id`, stored as `json`, last run at `last_run`.
pub fn enabled(id: &str, json: Value, last_run: Option<u64>) -> Enabled<Filter, Check> {
    let trigger = parse(json);
    Enabled {
        id: id.to_owned(),
        trigger,
        last_run,
    }
}

/// Sepolia's wallets in memory, native (3 decimals) and `usdc` (2)
/// configured, each spend priced [`FEE`], `send` registered.
pub struct World {
    pub wallets: Wallets,
    pub fees: Arc<Fees>,
    pub assets: Assets,
    pub network: Network,
    pub actions: Registry<Sent>,
}

impl World {
    pub fn new() -> Self {
        let network = Network::new(Chain::new("ethereum"), "sepolia");
        let native = AssetInfo::token(Asset::native(network.clone()), 3);
        let token = AssetInfo::token(asset(&network, TOKEN), 2);
        Self {
            wallets: Wallets::new(),
            fees: Arc::new(Fees::new(network.clone(), FEE.into())),
            assets: [native, token].into_iter().collect(),
            network,
            actions: Registry::new().add(Send),
        }
    }

    /// The same world pricing with `fees`.
    pub fn with_fees(mut self, fees: impl FnOnce(Fees) -> Fees) -> Self {
        self.fees = Arc::new(fees(Fees::new(self.network.clone(), FEE.into())));
        self
    }

    /// The gas wallet, untagged, holding 1 native.
    pub fn with_gas(mut self) -> Self {
        self.add(GAS, "native", (1_000, 1_000), &[]);
        self
    }

    /// `asset` of `address`: `confirmed` and `pending` atomic units, keyed,
    /// fully indexed, tagged `tags`.
    pub fn add(
        &mut self,
        address: &str,
        asset: &str,
        balances: (u64, u64),
        tags: &[&str],
    ) -> &mut Self {
        let one = self.index(address, asset, balances);
        self.wallets.add(one, tags);
        self
    }

    /// As [`World::add`], its key not kept here.
    pub fn add_watched(
        &mut self,
        address: &str,
        asset: &str,
        balances: (u64, u64),
        tags: &[&str],
    ) -> &mut Self {
        let mut one = self.index(address, asset, balances);
        one.keyed = false;
        self.wallets.add(one, tags);
        self
    }

    /// As [`World::add`], not fully indexed.
    pub fn add_behind(&mut self, address: &str, asset: &str, tags: &[&str]) -> &mut Self {
        let one = self.index(address, asset, (0, 0));
        self.wallets.add_behind(one, tags);
        self
    }

    fn index(&self, address: &str, asset: &str, (confirmed, pending): (u64, u64)) -> Index {
        let address = Address::new(self.network.clone(), address);
        let mut one = Index::new(address, self::asset(&self.network, asset));
        (one.confirmed, one.pending, one.keyed) = (confirmed.into(), pending.into(), true);
        one
    }

    /// Whether the trigger stored as `json` may be stored.
    pub fn validate(&self, json: Value) -> Result<(), Error> {
        let trigger: Definition = parse(json);
        trigger.validate(&self.actions, &self.network, &self.assets)
    }

    /// Why the trigger stored as `json` may not be stored.
    pub fn refusal(&self, json: Value) -> Error {
        self.validate(json).expect_err("refused")
    }

    /// One round over `triggers` at [`CONFIRMED`] and [`NOW`].
    pub fn round(&mut self, triggers: Vec<Enabled<Filter, Check>>) -> Vec<Decision<Sent>> {
        let network = self.network.clone();
        let round = Round::new(
            &self.actions,
            self.fees.clone(),
            &self.assets,
            network,
            CONFIRMED,
            NOW,
        );
        block_on(round.run(&mut self.wallets, triggers))
    }

    /// What the trigger stored as `json`, never run, decides alone.
    pub fn decide(&mut self, json: Value) -> Decision<Sent> {
        let mut decided = self.round(vec![enabled("one", json, None)]);
        decided.pop().expect("one decision")
    }

    /// The fully indexed ones of `ids`.
    pub fn this(&mut self, ids: &[String]) -> Selection {
        let found = block_on(self.wallets.select(&Selector::Ids(ids.to_vec()))).unwrap();
        Selection::of(self.network.clone(), found).unwrap()
    }

    /// The ids `selector` picks.
    pub fn select(&mut self, selector: Value) -> Vec<String> {
        let selector: Selector = parse(selector);
        let found = block_on(self.wallets.select(&selector)).unwrap();
        found.into_iter().map(|one| one.id).collect()
    }

    /// The ids `target` names with `this` in context; `None`: unknown.
    pub fn target(&mut self, target: Value, this: &[String]) -> Option<Vec<String>> {
        let target: Target = parse(target);
        let this = self.this(this);
        let mut root = self.context(None);
        let found = block_on(root.with(this).wallets(&target)).unwrap();
        found.map(|found| found.ids())
    }

    /// `amount` in atomic units of `asset` with `this` in context.
    pub fn amount(
        &mut self,
        amount: Value,
        this: &[String],
        asset: &str,
    ) -> anyhow::Result<Option<Decimal>> {
        let amount: Amount = parse(amount);
        let asset = self::asset(&self.network, asset);
        let this = self.this(this);
        let mut root = self.context(None);
        block_on(amount.atomic(&mut root.with(this), &asset))
    }

    /// Whether `check` holds over `this`, the trigger last run at `last_run`.
    pub fn holds(
        &mut self,
        check: Value,
        this: &[String],
        last_run: Option<u64>,
    ) -> anyhow::Result<bool> {
        let check: Logic<Check> = parse(check);
        let this = self.this(this);
        let mut root = self.context(last_run);
        block_on(check.holds(&mut root.with(this)))
    }

    fn context(&mut self, last_run: Option<u64>) -> Context<'_> {
        let (fees, assets, network) = (&*self.fees, &self.assets, &self.network);
        Context::new(&mut self.wallets, fees, assets, network, NOW, last_run)
    }
}

/// `text` as a decimal.
pub fn decimal(text: &str) -> Decimal {
    text.parse().expect("a decimal")
}
