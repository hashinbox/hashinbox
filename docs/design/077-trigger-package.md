# 077: `sdk/trigger`: the trigger engine as composable pieces

Status: package built (`sdk/trigger`, steps 1–3 of section 7) and the server
switched to it (step 4). Section 9 lists where the code differs from this design.
Paths under `apps/server/` unless noted;
line numbers are as of `bcbf1db`.

Today one trigger is evaluated by one 330-line struct
(`src/usecase/trigger.rs:36-253`). It matches on closed enums
(`src/models/trigger.rs:36-245`) and reaches into SQL directly
(`wallet_index::db::select`, `find_by_target`). An action registry sits on
top of it (`src/usecase/workers/trigger/actions.rs:25-108`). Adding a
condition, an amount or an action means editing all three.

This design splits the engine into small domains: **selection**, **amounts**,
**conditions**, **actions**, the **trigger** and the **round**. Each holds
its types, its rules, its errors and its tests, and the domains compose
through three traits. The package `sdk/trigger` holds them. The server
supplies storage, chooses which conditions and amounts make up its trigger
language, and registers its actions. It becomes configuration.

## Decisions first

1. **Three traits carry the composition**:
   - `conditions::Condition`: a yes/no question about the wallets being
     evaluated.
   - `amounts::Amount`: atomic units of an asset, or unknown.
   - `actions::Action<E>`: what a trigger does with its kept wallets,
     returned as effects of type `E`, never performed.

   Everything else implements, combines or feeds one of them.
2. **Combinators are generic.** `conditions::Logic<C>` (`all`/`any`/`not`)
   works over any condition and is one itself. `amounts::Arithmetic<A>`
   (`add`/`sub`/`min`/`max`/`times`) works over any amount and is one
   itself. The package ships the building blocks: `Compare`, `Since`,
   `Value`, `Balance`, `Fee`.
3. **The trigger language is the server's.** Its `Filter`, `Check` and
   `AmountTerm` enums choose the building blocks and give the stored JSON
   its shape. Today's JSON does not change. A new building block is a
   server type plus one enum variant.
4. **Actions are the server's.** The package defines the `Action` trait,
   the `Registry` and the stored `Call { name, input }`. It never parses an
   input: each action declares its own `Input` type, and the registry
   decodes and validates it. The server's `send` returns broadcast
   `tasks::Job`s.
5. **One store trait, `trigger::Store`**: a network's wallet indexes at
   its confirmed block, from one consistent read. Async with
   `base::Future`, as `base`'s stores are. The confirmed position and the
   enabled triggers are inputs to the round, not reads behind the trait.
6. **No write trait.** A round returns one `Decision` per trigger (its last
   round and its effects). The server writes them, jobs included, in one
   transaction.
7. **One error, classed.** `error::Error { class, message }`: every
   piece returns it directly, its `Class` saying what kind of thing is
   wrong and its message today's text. No roll-up, no `From` chains. There is no other
   "invalid" file.
8. **Fees are the chain's business.** The package takes the
   `Arc<dyn base::FeeEstimator>` that `factory.fees()` hands out and calls
   `estimate(&Spends)`. That is each chain's `Cached` estimator
   (`sdk/chains/{ethereum,solana,bitcoin}/src/fee_cache.rs`), which decides
   what to reuse. The package has no fee cache of its own.
9. **No change to `sdk/base`, `sdk/wallets` or `packages/crypto`.**

## 1. The package, domain by domain

```text
sdk/trigger/
  Cargo.toml         base, serde, serde_json, anyhow; features `openapi` (utoipa), `testing`
  src/lib.rs         how the pieces fit together; re-exports
  src/selection.rs   Index, BalanceKind, Selection, Selector, Target
  src/store.rs       the Store trait (a network's indexes at its confirmed block, one consistent read); CachedStore
  src/amounts.rs     the Amount trait; Value, Balance, Fee, Arithmetic (add/sub/min/max/times)
  src/conditions.rs  the Condition trait; Compare (Aggregate, Operator), Since, Logic (all/any/not)
  src/actions.rs     the Action trait, Registry, Call
  src/trigger.rs     Trigger (selector, filter, check, action), its rules and how one trigger decides; Context
  src/error.rs       Error { class, message } and Class: the one error every rule returns
  src/round.rs       Round over every enabled trigger at one block; Enabled, Decision, Stats, LastRound
  src/testing/       (feature testing) the two test doubles
    mod.rs           docs and re-exports
    memory.rs        `Wallets`: the in-memory Store, the reference for what each selector means
    fee.rs           `Fees`: a fixed-price FeeEstimator that keeps what it was asked
  tests/             end-to-end tests through the public API (section 6)
  examples/sweep.rs  the "Token sweep with gas funding" pair, one round, offline
```

Each file holds one whole domain: its trait, its pieces, its rules, its
error and its tests. That is about 1,000 lines over 8 files, 80–200 each.

`Context` lives in `trigger.rs`: it is the state of one trigger
deciding (its wallets, its last run), and the round only creates one per
trigger.

### Names: what changed and why

| Before (v2 of this doc) | Now | Why |
|---|---|---|
| `Predicate` | `Condition` | the plain word for "when this holds" |
| `Expr<L>` | `Arithmetic<A>` | says what it does: add, sub, min, max, times |
| `Scope` | `Context` | it is the state of evaluating one trigger: its wallets, its last run, the round's reads |
| `Indexes` (port), `Snapshot` (v3) | `Store` | what the round reads wallet indexes from, as `tasks::Store` is what the queue reads jobs from |
| `CachedSnapshot` (v3) | `CachedStore` | a `Store` over another that answers each selector once per round |
| `Held` | `BalanceKind` | which balance, confirmed or pending; avoids a clash with the `Balance` amount |
| `Actions<P>` | `actions::Registry<E>` | a registry of actions by name |
| plans, `P` | effects, `E` | what the action asks its caller to carry out once the round is written |
| `Decided<P>` | `round::Decision<E>` | a noun: what one trigger decided this round |
| `Gate`, `Leaf` (server) | `Check`, `AmountTerm` | match the JSON key `check` and say what they are |
| `delegate!` | removed | a hand-written `match` is clearer than a macro (section 3) |
| `invalid.rs`, `Invalid` | `error::Error` with a `Class` | one error, its class says what kind of thing is wrong |
| `Evaluation` (v3) | `Context` | the state one trigger decides in; was `Evaluation` |
| `wallets.rs` | `selection.rs` | the module of `Selection`, `Selector` and their store |
| `pricing.rs`, `Pricing` | removed | each chain's `Cached` estimator already reuses answers |

Names that stay, each in one line:

- `Index`, `Selection`, `Selector`: the words the API and the server already
  use for these exact things (`GET /v1/wallets/…/indexes`, `selector`).
- `Target`: the JSON forms `{"this": …}` / `{"select": …}` that every
  compare, amount and send names.
- `Logic`: the type of `all`/`any`/`not`, in `conditions.rs`.
- `Compare`, `Since`, `Value`, `Balance`, `Fee`: the JSON keys themselves.
- `Call`: a stored call to an action (`{"name", "input"}`).
- `Enabled`: an enabled trigger with its id and last run, which is what a
  round takes.

## 2. The domains and their contracts

| Domain | Types | Responsibility | In → out | Composes with |
|---|---|---|---|---|
| selection | `Index`, `BalanceKind`, `Selection`, `Selector`, `Target` | which wallets, and their balances at the confirmed block | selector or (address, asset) pairs → indexes | every amount and condition, through `Context::wallets` |
| amounts | `Amount`, `Value`, `Balance`, `Fee`, `Arithmetic<A>` | how much, in atomic units of an asset | (`Context`, `Asset`) → `Option<Decimal>` | `Compare`, actions |
| conditions | `Condition`, `Compare<A>`, `Since`, `Logic<C>` | whether something holds | `Context` → `bool` | filters and checks of a trigger |
| actions | `Action<E>`, `Registry<E>`, `Call` | the seam a trigger acts through; decodes and checks calls | (input, `Context`) → `Vec<E>` | `Trigger`, `Round` |
| trigger | `Trigger<F, C>`, `Context` | select → keep each index by `F` → check by `C` → call | `Context` → kept wallets + effects | `Round` |
| error | `Error`, `Class` | why something cannot be stored | — | every `validate` |
| store | `Store`, `CachedStore` | reads a network's indexes at its confirmed block, one consistent read; each selector once per round | selector or (address, asset) pairs → indexes | `Context`, `Round` |
| round | `Round<E>`, `Enabled`, `Decision<E>`, `Stats`, `LastRound` | every enabled trigger at one block; one refused trigger never stops the others | (`Store`, triggers) → `Vec<Decision<E>>` | the server's worker |

We considered making these into traits too, and chose not to:

- **`Target`**: its forms are the whole vocabulary of "which wallets" over
  the store. A new form would need a new store method anyway.
- **`Selector`**: it is the store's query language, translated by each
  store (SQL in the server, a `match` in `testing/memory.rs`).
- **Pipeline stages**: three lines in `Trigger::decide`.

### The traits

```rust
// conditions.rs
/// Whether something holds about the wallets being evaluated.
pub trait Condition: Send + Sync {
    /// A term it cannot know (a sibling not fully indexed, no asset) is
    /// `false`; a read error stays an error.
    fn holds<'a>(&'a self, on: &'a mut Context<'_>) -> Future<'a, anyhow::Result<bool>>;
    /// The rules it must keep to be stored.
    fn validate(&self) -> Result<(), Error>;
    /// Whether it reads `this`: a filter may only use ones that do.
    fn reads_this(&self) -> bool;
}

// amounts.rs
/// An amount in atomic units of `asset`; `None`: a term is unknown.
pub trait Amount: Send + Sync {
    fn atomic<'a>(&'a self, on: &'a mut Context<'_>, asset: &'a Asset)
        -> Future<'a, anyhow::Result<Option<Decimal>>>;
    fn validate(&self) -> Result<(), Error>;
    fn reads_this(&self) -> bool;
    /// Its fixed display value when it is one: a count compares only with a whole one.
    fn constant(&self) -> Option<&Decimal> { None }
}

// actions.rs
/// What a trigger does once its check holds: effects for its caller to
/// carry out after the round is written, never effects of its own (it runs
/// inside the round's read).
pub trait Action<E>: Send + Sync + 'static {
    /// Its name in `{"name": …, "input": …}`.
    const NAME: &'static str;
    type Input: DeserializeOwned + Send + Sync;
    /// Refuses an input it could never run, before the trigger is stored.
    fn validate(&self, input: &Self::Input, network: &Network, assets: &Assets) -> Result<(), Error>;
    /// What to do now, `on.this()` being the kept wallets.
    fn run<'a>(&'a self, input: &'a Self::Input, on: &'a mut Context<'_>)
        -> Future<'a, anyhow::Result<Vec<E>>>;
}

// store.rs
/// Reads one network's wallet indexes at its confirmed block, all from one
/// consistent read: only fully indexed ones (indexed through a live block at
/// or above the confirmed position, the universe `Selector::Not` is relative
/// to), each with its balances at that position.
pub trait Store: Send {
    fn select<'a>(&'a mut self, selector: &'a Selector) -> Future<'a, anyhow::Result<Vec<Index>>>;
    /// Of `wanted`, the fully indexed ones; the rest are left out.
    fn find<'a>(&'a mut self, wanted: &'a [(Address, Asset)]) -> Future<'a, anyhow::Result<Vec<Index>>>;
}
```

### What an evaluation offers (`trigger.rs`)

```rust
impl<'r> Context<'r> {
    /// The wallets being evaluated: `this`.
    pub fn this(&self) -> &Selection;
    /// The same evaluation with `this` replaced: a filter per index, an amount per source.
    pub fn with(&mut self, this: Selection) -> Context<'_>;
    /// The wallets `target` names, read once per round (`select` by its selector alone).
    pub async fn wallets(&mut self, target: &Target) -> anyhow::Result<Option<Selection>>;
    /// The chain's estimator, as `factory.fees()` handed it out.
    pub fn fees(&self) -> &dyn FeeEstimator;
    /// `value` in display units of `asset` → atomic.
    pub fn atomic(&self, asset: &Asset, value: &Decimal) -> anyhow::Result<Decimal>;
    pub fn network(&self) -> &Network;
    /// Unix ms.
    pub fn now(&self) -> u64;
    /// When this trigger's latest run started; `None`: never.
    pub fn last_run(&self) -> Option<u64>;
}
```

### Errors: one, classed (`error.rs`)

```rust
pub struct Error { pub class: Class, message: String }   // Display: the message
pub enum Class {
    EmptySelection, Malformed, Impossible, Redundant, OutOfRange,
    FilterIgnoresWallet, UnknownAction, BadInput, Refused, NotConfigured,
}
```

Each rule returns `Error::new(class, message)` itself; nothing wraps or
converts. The messages are today's (`src/models/trigger.rs:296-482`,
`wallet_index_selector.rs:37-53`), so the API answers and the tests that
match on them do not change. The API maps the class to a status:

| Class | What is wrong | Status |
|---|---|---|
| `EmptySelection` | a selection picks nothing or everything: empty ids, empty "any of" / "all of", empty asset | 400 |
| `Malformed` | a name not well formed: a tag | 400 |
| `Impossible` | a condition that can never hold: an empty "any of" | 400 |
| `Redundant` | a "not" right inside a "not" (selector or condition) | 400 |
| `OutOfRange` | a count against a fractional number, a `times` factor ≤ 0 | 400 |
| `FilterIgnoresWallet` | a filter condition that never reads the wallet it tests | 400 |
| `UnknownAction` | no action of the name is registered | 400 |
| `BadInput` | an action's input does not decode (`"send input: …"`) | 400 |
| `Refused` | an action refuses its input | 400 |
| `NotConfigured` | an action's input names an asset not configured here | 404 |

Old variant → class: `wallets::Error::{EveryWallet, NoIds, NoWallet,
NoAsset}` → `EmptySelection`; `BadTag` → `Malformed`; `DoubleNot` (both) →
`Redundant`; `conditions::Error::NeverHolds` → `Impossible`;
`CountNotWhole` and `amounts::Error::FactorNotPositive` → `OutOfRange`;
`trigger::Error::FilterIgnoresThis` → `FilterIgnoresWallet`;
`actions::Error::{Unknown, Input, Refused, NotConfigured}` →
`UnknownAction`, `BadInput`, `Refused`, `NotConfigured`. The wrapping
variants (`Wallets`, `Amount`, `Selector`, `Filter`, `Check`, `Action`) are
gone: the inner error is returned as it is.

### The trigger and the round

```rust
// trigger.rs
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trigger<F, C> {
    pub selector: Selector,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Logic<F>>,
    pub check: Logic<C>,
    pub action: Call,
}

impl<F: Condition, C: Condition> Trigger<F, C> {
    pub fn validate<E>(&self, actions: &Registry<E>, network: &Network, assets: &Assets) -> Result<(), Error>;
}

// round.rs
impl<'a, E> Round<'a, E> {
    pub fn new(actions: &'a Registry<E>, fees: Arc<dyn FeeEstimator>, assets: &'a Assets,
               network: Network, confirmed: u64, now: u64) -> Self;
    /// Every trigger in the order given.
    pub async fn run<F: Condition, C: Condition>(self, store: &mut dyn Store,
               triggers: Vec<Enabled<F, C>>) -> Vec<Decision<E>>;
}

pub struct Enabled<F, C> { pub id: String, pub trigger: Trigger<F, C>, pub last_run: Option<u64> }
pub struct Decision<E> { pub trigger: String, pub last: LastRound, pub effects: Vec<E> }
```

`Stats { selected, kept, checked, jobs }` and `LastRound { at, confirmed,
stats, refused }` keep their JSON (`src/models/trigger.rs:386-415`), and
`jobs` is `effects.len()`. They derive `utoipa::ToSchema` behind feature
`openapi`. `RunOutcome` stays in the server (`src/api/triggers.rs:411-440`):
it folds queue job states.

## 3. The server after: configuration

### The language (`src/models/trigger.rs`, the stored vocabulary, ~50 lines)

```rust
/// The amount terms this server offers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AmountTerm { Value(amounts::Value), Balance(amounts::Balance), Fee(amounts::Fee) }
pub(crate) type Amount = amounts::Arithmetic<AmountTerm>;
pub(crate) type Compare = conditions::Compare<Amount>;

/// What a filter may test, per index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Filter { Compare(Compare) }

/// What a check may test, over the kept set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Check { Since(conditions::Since), Compare(Compare) }

impl conditions::Condition for Check {
    fn holds<'a>(&'a self, on: &'a mut Context<'_>) -> Future<'a, anyhow::Result<bool>> {
        match self { Self::Since(one) => one.holds(on), Self::Compare(one) => one.holds(on) }
    }
    fn validate(&self) -> Result<(), trigger::Error> {
        match self { Self::Since(one) => one.validate(), Self::Compare(one) => one.validate() }
    }
    fn reads_this(&self) -> bool {
        match self { Self::Since(one) => one.reads_this(), Self::Compare(one) => one.reads_this() }
    }
}
// `Filter` and `AmountTerm`: the same `match`, one arm per variant.

pub(crate) type Definition = ::trigger::Trigger<Filter, Check>;
```

The JSON stays as it is: `{"value": "1"}`, `{"balance": {...}}`,
`{"fee": {...}}`, `{"add": [a, b]}`, `{"times": [a, "1.25"]}`,
`{"since": {"seconds": n}}`, `{"compare": {...}}`, `{"all": [...]}`. The row
(`Trigger`, `db`) stays (`:417-609`). Its columns are typed
`Json<Selector>`, `Option<Json<Logic<Filter>>>`, `Json<Logic<Check>>`,
`Json<Call>`, and `db::find_enabled` returns
`Vec<round::Enabled<Filter, Check>>`.

### The store (`src/usecase/trigger/store.rs`, ~50 lines)

```rust
/// `trigger::Store` over one read transaction: every statement is `wallet_index`'s.
pub(crate) struct Sqlite<'c> { connection: &'c mut SqliteConnection, network: Network, confirmed: i64 }

impl ::trigger::Store for Sqlite<'_> {
    fn select<'a>(&'a mut self, selector: &'a Selector) -> Future<'a, anyhow::Result<Vec<Index>>> {
        let found = wallet_index::db::select(self.connection, &self.network, selector, self.confirmed);
        Box::pin(std::future::ready(found))
    }
    fn find<'a>(&'a mut self, wanted: &'a [(Address, Asset)]) -> Future<'a, anyhow::Result<Vec<Index>>> {
        let found = wallet_index::db::find(self.connection, &self.network, wanted, self.confirmed);
        Box::pin(std::future::ready(found))
    }
}
```

`wallet_index::db::select` keeps its query (`src/models/wallet_index.rs:747-846`).
`Selector::sql` (`src/models/wallet_index_selector.rs:64-90`) becomes the
free function `wallet_index::db::picked`, and the file is deleted. It
answers `selection::Index`, decoded once (today's `spendable`/`held` at
`:144-166`), and refuses a negative confirmed balance as today (`:838-842`).
`find_by_target` (`:855-878`) becomes `find`: one query, the pairs as ids.

### The action (`src/usecase/trigger/send.rs`, ~110 lines)

```rust
/// `send`'s input, the stored JSON of today.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendInput { source: Target, asset: String, destination: Target, amount: models::trigger::Amount }

/// `send`: moves funds, one broadcast job per sender.
pub(crate) struct Transfer;

impl actions::Action<Job> for Transfer {
    const NAME: &'static str = "send";
    type Input = SendInput;

    fn validate(&self, send: &SendInput, network: &Network, assets: &Assets) -> Result<(), trigger::Error> {
        // targets named on purpose, the amount's rules, not this → this;
        // `NotConfigured` when the sent asset is not configured
    }

    fn run<'a>(&'a self, send: &'a SendInput, on: &'a mut Context<'_>) -> Future<'a, anyhow::Result<Vec<Job>>> {
        Box::pin(async move {
            let asset = Asset { network: on.network().clone(), id: send.asset.clone() };
            let Some(sources) = on.wallets(&send.source).await? else { bail!("a source is not fully indexed") };
            let Some(destinations) = on.wallets(&send.destination).await? else { bail!("a destination is not fully indexed") };
            // ensure! one asset, not many↔many; walk the many side (today's
            // actions.rs:176-197), each alone:
            //   let amount = send.amount.atomic(&mut on.with(each.only(one)), &asset).await?;
            // skip ≤ 0 or unknown, ensure! keyed, group by sender
            by_sender.into_iter()
                .map(|(from, transfers)| Broadcast::job(&SendIntent::new(&from, &asset, transfers)))
                .collect()
        })
    }
}
```

The planning logic is today's `Transfer::run` (`actions.rs:153-235`). It
now reads wallets and amounts through `Context` instead of the old
`Trigger<'c>`.

### The composition root (`src/main.rs::boot`, +3 lines)

```rust
let actions = Arc::new(actions::Registry::new().add(usecase::trigger::Transfer));
let executor = Executor::new(queue.clone(), tick)
    // … index, expire, prune, broadcast as today …
    .add(trigger::Round::new(payments.clone(), db.clone(), queue.clone(), actions.clone(), logger));
let state = Arc::new(AppState { payments, db, queue, actions });
```

### The worker (`src/usecase/workers/trigger.rs`, ~100 lines)

Its policy, topic and the index job's `enqueue` are unchanged
(`src/usecase/workers/trigger/mod.rs:60-77`). `run` is wiring:

```rust
async fn run(&self, cx: tasks::Context<'_, Target, ()>) -> Ack {
    let network = cx.input().network();
    let round = async {
        let service = self.payments.service(&network)?;
        // The chain's cached estimator: it decides what it reuses.
        let (actions, fees, assets) = (self.actions.clone(), service.wallets.factory.fees(), service.assets.clone());
        let (now, runtime, at) = (tasks::now(), Handle::current(), network.clone());
        // One read transaction: confirmed, triggers and every wallet read agree.
        let decided = self.db.read(move |connection| {
            let Some(confirmed) = network_index::db::find_confirmed(connection, &at)? else { return Ok(None) };
            let triggers = trigger::db::find_enabled(connection, &at)?;
            let mut wallets = usecase::trigger::Sqlite::new(connection, &at, confirmed);
            let round = ::trigger::round::Round::new(&actions, fees, &assets, at.clone(), uint(confirmed)?, now);
            Ok(Some(runtime.block_on(round.run(&mut wallets, triggers))))
        });
        let Some(decided) = decided.await? else { return anyhow::Ok(Ack::Idle) };
        self.log_refusals(&network, &decided);
        if decided.is_empty() { return anyhow::Ok(Ack::Idle) }
        let acted = decided.iter().any(|one| !one.effects.is_empty());
        self.db.write(move |connection| save(connection, now, &decided)).await?;
        if !acted { return anyhow::Ok(Ack::Idle) }
        self.queue.notify();
        anyhow::Ok(Ack::Done)
    };
    round.await.unwrap_or_else(|error| Ack::Retry(format!("{error:#}")))
}

/// Every trigger's last round, and each acting one's jobs and run: one write.
fn save(connection: &mut SqliteConnection, now: u64, decided: &[Decision<Job>]) -> anyhow::Result<()> {
    let now = int(now)?;
    for one in decided {
        trigger::db::set_last_round(connection, &one.trigger, &one.last)?;
        if one.effects.is_empty() { continue }
        let jobs = one.effects.iter()
            .map(|job| Ok(task::db::insert(connection, &Row::try_from(job)?, now)?.to_string()))
            .collect::<anyhow::Result<Vec<_>>>()?;
        trigger_run::db::insert(connection, &TriggerRun::new(&one.trigger, now, jobs, one.last.stats))?;
    }
    Ok(())
}
```

### The API (`src/api/triggers.rs`)

`Definition::trigger` (`:116-161`) builds a `models::trigger::Definition`
and calls `definition.validate(&state.actions, &network, &service.assets)`,
mapping its `Class` to a status as in section 2. The API no longer names `send`,
`SendInput` or an action name.

### The server's trigger code, before → after

| | Before | After |
|---|---|---|
| models (`trigger.rs`, `wallet_index_selector.rs`, trigger part of `wallet_index.rs`) | ~1,000 | ~320 (row, db, language, `picked`, `find`) |
| `usecase/trigger.rs` + `workers/trigger/{mod,actions}.rs` | ~760 | ~260 (store 50, `send` 110, worker 100) |
| `api/triggers.rs` | 527 | ~500 |
| `main.rs` | — | +3 |

## 4. Worked examples

### A: "Token sweep with gas funding" (`ui/src/pages/triggers/_components/forms.ts:456-525`)

Two triggers on the deposit wallets. Each line is a building block; none
is specific to this template.

```text
fund-gas
  selector  Selector::All[Tag(deposit), Asset(native)]                      → Store::select
  filter    Logic::All[                                                       each index alone (Context::with)
              Filter::Compare(Compare{ of: This{asset: token}, Sum, Above, Value 0 }),
              Filter::Compare(Compare{ of: This{}, Sum, balance: Pending, Below,
                                       Fee{ of: This{asset: token}, to: chosen } }) ]
  check     Logic::All[]                                                      always
  action    Call{ "send", { source: chosen, asset: native, destination: This{},
              amount: Max[ Sub[ Times[Fee{This{asset: token}, chosen}, 1.25], Balance{This, Pending} ], Value 0 ] } }
sweep-token
  selector  Selector::All[Tag(deposit), Asset(token)]
  filter    Logic::All[ Compare{ This{}, Sum, Above, Value 0 },
                        Compare{ This{asset: native}, Sum, AtLeast, Fee{ This{}, chosen } } ]
  check     Logic::All[]
  action    Call{ "send", { source: This{}, asset: token, destination: chosen, amount: Balance{This} } }
```

How the pieces evaluate it:

- `This{asset: token}` goes through `Context::wallets`, then
  `Store::find`. It is unknown (the compare is `false`) when the token
  index is not fully indexed (R6).
- `Fee` builds `Selection::spends(to)` and calls `on.fees().estimate`. A
  wallet holding nothing costs 0 and asks nothing (R8).
- Ethereum's `Cached` (`sdk/chains/ethereum/src/fee_cache.rs:12-28`) keeps
  the gas price and the gas per (asset, recipient holds it), so the
  filter's fee and the action's `Times[Fee…]` get one answer from the node.
- `send` is the server's: its `amount` is an `Arithmetic<AmountTerm>`,
  evaluated per destination with `on.with(each.only(one))`.

### B: a new condition, then a new action, both in the server only

A check that holds only in a UTC hour window,
`{"window": {"from": 9, "to": 17}}`:

```rust
// src/models/trigger.rs
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Window { from: u8, to: u8 }

impl conditions::Condition for Window {
    fn holds<'a>(&'a self, on: &'a mut Context<'_>) -> Future<'a, anyhow::Result<bool>> {
        let hour = u8::try_from(on.now() / 3_600_000 % 24).unwrap_or(0);
        Box::pin(std::future::ready(Ok((self.from..self.to).contains(&hour))))
    }
    fn validate(&self) -> Result<(), trigger::Error> {
        if self.from < self.to && self.to <= 24 { return Ok(()) }
        Err(Error::new(Class::Impossible, "a window ends after it starts"))
    }
    fn reads_this(&self) -> bool { false }
}

pub(crate) enum Check { Since(conditions::Since), Compare(Compare), Window(Window) }   // + one variant
// + one arm in each of Check's three matches
```

`Logic<Check>` accepts it under `all`/`any`/`not` at once. Because
`reads_this` is false, putting it in a filter is refused by the existing
`FilterIgnoresThis` rule. No package change.

An action `split` that pays a source's balance to several destinations by
shares, `{"name": "split", "input": {"source": …, "asset": …, "shares":
[{"to": …, "share": "0.7"}, …]}}`:

```rust
// src/usecase/trigger/split.rs
impl actions::Action<Job> for Split {
    const NAME: &'static str = "split";
    type Input = SplitInput;
    fn validate(&self, input: &SplitInput, network: &Network, assets: &Assets) -> Result<(), trigger::Error> { /* shares in (0, 1], sum ≤ 1 */ }
    fn run<'a>(&'a self, input: &'a SplitInput, on: &'a mut Context<'_>) -> Future<'a, anyhow::Result<Vec<Job>>> {
        // on.wallets(source), Balance{This}.atomic(...), one Broadcast::job with a transfer per share
    }
}
// src/main.rs
let actions = Arc::new(actions::Registry::new().add(Transfer).add(Split));
```

The API validates it, rounds run it and runs record its jobs, with no other
change.

## 5. Correctness requirements to keep

| # | Requirement | Where after | Tested today |
|---|---|---|---|
| R1 | Decide at the confirmed position, never the tip; no round while unknown | worker reads `find_confirmed`, `Round::new(confirmed)` | universe only: `tests/pipe.rs:204,239`; **"confirmed, not tip" untested** |
| R2 | Only fully indexed indexes; `Not` relative to them | `Store` contract, `wallet_index::db::select` | `pipe.rs:204`, `:281` |
| R3 | `BalanceKind::Confirmed` = spendable at confirmed, `Pending` = once pending lands | `selection::Index`, view `wallet_index_with_balance` | `pipe.rs:307`, `:351`, `:1094`; `tests/storage.rs:1050` |
| R4 | One consistent read per round | the worker's one `db.read` around `Round::run` | implicit, no test |
| R5 | Filter per index, check over the kept set; written order, short-circuit; errors stay errors | `Trigger::decide`, `conditions::Logic` | `pipe.rs:991`, `:813` |
| R6 | Unknown is `false` (sibling missing, no asset, min/max of none) | `Context::wallets`, `Compare` | `pipe.rs:787`, `:504` |
| R7 | `since` from the latest run; no run passes | `Since`, `Context::last_run` | `pipe.rs:869`, `:927` |
| R8 | Fee via `FeeEstimator` over `Spends`, one recipient priced as is, else a new address; holding nothing costs 0 and asks nothing | `amounts::Fee`, `Selection::spends` | `pipe.rs:687`, `:721`, `:747` |
| R9 | Counts compare with whole numbers, at create and at decide | `Compare::validate` via `Amount::constant` | `tests/routes.rs:708`, `pipe.rs:504` |
| R10 | `value` display → atomic by the asset's decimals; too many decimals errors | `amounts::Value`, `Context::atomic` | `pipe.rs:608`, `:624` |
| R11 | Send shapes, keyed sources, per-sender grouping, skip ≤ 0 | server `Transfer` | `pipe.rs:1023` |
| R12 | A refusal is per trigger; stats keep what was counted before it | `Round::run` | **untested** |
| R13 | A last round for every enabled trigger, every round | `Decision.last`, `save` | `tests/cases/backlog.rs:61-63` |
| R14 | A run only when effects exist, with job ids and stats | `save` | `routes.rs:654` |
| R15 | One write per round, all or none | `save` in one `db.write` | `cases/partial.rs` |
| R16 | Unique round per network; skipped while a later job of the network is unfinished | worker `POLICY`, topic | `cases/skipped.rs`, `cases/backlog.rs` |
| R17 | Stored JSON unchanged | the language enums, `Logic`/`Arithmetic` forms | `pipe.rs:884` |
| R18 | Definition rules: wallets on purpose, filter reads `this`, check can hold, no `this`→`this` send | each domain's `validate`, rolled up by `Trigger::validate`; `Transfer::validate` | `src/models/trigger.rs:642-704` |

### Bugs and ambiguities (fixed in their own steps, after the move)

- **B1 A compare can use the wrong asset.** `compare` takes `of.asset()` or
  else the first index of `this` (`src/usecase/trigger.rs:151-159`), and a
  `value` is converted with that asset's decimals. Picture a `select` of a
  native index checked against a `this` of token indexes. Fix in
  `conditions.rs` (`Compare`): the asset is the single asset of `of`'s
  resolved indexes; mixed assets are a refusal; an empty set falls back to
  the named asset, then `this`'s.
- **B2 Mixed-asset sums.** A `{tag}` selector over native and token indexes
  sums both. B1's fix refuses it.
- **B3 Fees asked twice (re-checked; no longer a bug).** Each `Fee` term
  still calls `estimate` (`src/usecase/trigger.rs:203-227`). But the
  estimator is the chain's `Cached`, handed out by `factory.fees()` and
  signed with by every builder (`sdk/chains/*/src/fee_cache.rs`):
  - Ethereum keeps the gas price and the gas per (asset, recipient holds
    it);
  - Solana keeps the blockhash and signature price, and rent for good;
  - Bitcoin keeps the fee rate and reads each sender's coins from the
    store, not the node.

  Within a round, check and action get the same price, unless the TTL ends
  between them. Then the broadcast signs at the newer price, which the
  `FeeEstimator` contract allows (it may overpay slightly, never
  underpay). What is left is a cost, not a bug: one holdings read per
  `Fee` term (one store read on Bitcoin). No package cache.
- **B4 The `Select` target cache is keyed by `this`'s ids** (`:245`). A
  filter re-reads the same selector once per index. `Context::wallets`
  keys a `Select` by the selector alone. Under one consistent read the results are
  equal, so this ships with the move.
- **B5 Empty source or destination is a silent no-op**
  (`actions.rs:195-197`, 075 F8). Fix in the server's `Transfer`.
- **B6 Value precision is checked only at decide time** (075 F8). Fix: the
  `Value`/`Compare` validation, given assets where the asset is named.
- **B10 A `routes.rs` fixture won't validate.** `tests/routes.rs:666-668`
  seeds `{"name":"send","input":{}}`. `Call` keeps the input as JSON, so
  the row still decodes, but `Trigger::validate` would refuse it. The
  fixture should carry a full input.
- **B11 A fee is native whatever the compare reads.** `Fee` ignores the
  asset it is asked in (`src/usecase/trigger.rs:203-227`, ported as is in
  `amounts::Fee`). `{"compare": {"of": {"this": {"asset": token}}, …,
  "value": {"fee": {}}}}` compares token units with native ones, and
  `{"sub": [{"balance": …token…}, {"fee": {}}]}` mixes them. Fix with B1:
  a fee in a non-native compare or sum is a refusal.
- **B12 Out-of-range waits and negative values pass create.** A `since`
  whose seconds overflow Unix ms (`> i64::MAX / 1000`) and a `{"value":
  "-1"}` are refused only at decide time, as a refusal every round. Fix with
  B6: `Since::validate` and `Value::validate` refuse them.
- **B13 A scheduled transfer ignores the balance.** "Scheduled transfer"
  sends `{"value": "0.1"}` once a day whatever the treasury holds
  (`tests/scheduled.rs`): below it, a job is queued that cannot succeed,
  every day. Fix in the template (a check that the balance covers the amount
  and its fee), not the package.
- **B14 An empty `this` hides the asset of a selector compare.** A compare
  over a `select` that names no asset takes the asset of `this`'s first
  index; with no wallet in context it is `false`, even `sum at_least 0`
  (`tests/conditions.rs`). Fixed with B1 (the asset of `of`, then the named
  one).

## 6. Testing plan

**`sdk/trigger`**: end to end, through the public API only, in
`sdk/trigger/tests/` (`src/` keeps no tests: nothing private needs one). Each
test parses triggers from their stored JSON, validates them, runs a `Round`
over `testing::Wallets` with `testing::Fees` and a recording `send`, and
asserts decisions, effects and stats. `tests/common/mod.rs` holds the shared
setup: a trigger language as the server writes it, that `send`, the UI's
templates as stored JSON, and a `World` of Sepolia wallets.

| File | Covers |
|---|---|
| `forward.rs` | "Native forward": over twice the fee, each alone, priced to the chosen wallet, a cheaper known recipient, an unkeyed source refused |
| `consolidate.rs` | "Consolidation over a threshold": below / at the threshold, wallets not worth their fee not counted, pending not counted |
| `sweep.rs` | "Token sweep with gas funding", both triggers: 1.25 × headroom rounded up, no second top-up while pending gas ≥ 1 × fee, top-up by the rest, sweep at one fee, nothing, a token index behind, never both |
| `scheduled.rs` | "Scheduled transfer": never run, an hour ago, a day ago, a moment short, only the treasury |
| `amounts.rs` | values by decimals, too many decimals, unconfigured; confirmed vs pending; fee to one recipient / several / none / missing, holders only, nothing costs 0 and is never asked, a failing estimate; `times` rounding; add/sub/min/max; unknown propagates |
| `conditions.rs` | every aggregate and operator; counts with whole numbers only; pending; siblings; empty sets; `all`/`any`/`not` in order, stopping early (a later condition that would fail is never reached; a later fee is never asked); errors under `not`; `since` |
| `selection.rs` | ids, asset, tag, untagged, all/any/not, behind never picked; targets `this`, `this` in another asset (missing, behind), a selector; spends, address, network |
| `round.rs` | order kept, empty round, a refusal not stopping the others and keeping its stats, a last round for every trigger, jobs = effects, each selector read once per round, `since` per trigger, a failing estimate, refusals at run |
| `validation.rs` | one test per `Class` with its message; every template accepted; which part is checked first |
| `json.rs` | every stored form (selector, target, amount, check, filter, call, trigger, last round) round-trips byte for byte; templates; defaults written out |

**Server** (SQL, HTTP, workers):

- `pipe.rs` keeps the store facts (`:178`, `:204`, `:239`, `:281`, `:307`,
  `:351`, `:747`, `:927`, `:974`, the SQL half of `:1094`) through
  `usecase::trigger::Sqlite`.
- `Transfer` keeps its test (`:1023`), over the real store.
- `routes.rs` and `cases/*` stay unchanged and keep R13–R16 green.

## 7. Migration (each step builds and keeps `cargo test` green)

1. **Wallets**, added: `sdk/trigger` with `selection.rs`, `store.rs`, `testing/` and their tests. The
   server is untouched.
2. **Amounts and conditions**, added: `amounts.rs`, `conditions.rs`,
   and `Context` in `trigger.rs`, with the moved evaluation tests over
   `testing/`.
3. **Actions, trigger, round**, added: `actions.rs`, the rest of
   `trigger.rs`, `round.rs`, and their tests.
4. **The server switches.** The language in `models/trigger.rs`; the
   store in `usecase/trigger/store.rs`; `Transfer` in
   `usecase/trigger/send.rs`; the worker, API and `main.rs` as in
   section 3. Deleted: `usecase/trigger.rs` (the old one),
   `workers/trigger/actions.rs`, `models/wallet_index_selector.rs`,
   `Selection`, `find_by_target`, `find_by_ids`. `pipe.rs` drops what
   moved; the `routes.rs` fixture is fixed (B10). The JSON is unchanged,
   and `pipe.rs:884`, `routes.rs` and `cases/*` guard it.
5. Then, one step each, with a test, once agreed: B1 and B2, B5, B6.

## 8. Open questions (recommendation first)

1. **Crate name `trigger`** clashes with `models::trigger`,
   `usecase::trigger` and `workers::trigger`. *Recommend:* keep it as asked
   and write `::trigger::` in those three modules.
2. **Should the package also ship a standard language** (`Filter`, `Check`,
   `AmountTerm`) for third parties? *Recommend:* not yet. The server is the
   only user; promote its enums when a second one appears.
3. **The effect type `E`.** *Recommend:* `tasks::Job`. Every effect today is
   a job, so the round's one write stays one write. A server enum only
   when an action needs an effect that is not a job.
4. **Conditions that need data an evaluation lacks** (eval.md's `Idle`: the
   last run settled). *Recommend:* grow `Context::last_run` into a
   `LastRun { started_at, settled }` when `Idle` is built; do not make
   `Context` generic now.
5. **Async store driven by one `block_on`, or a sync one?**
   *Recommend:* async. It is `base`'s style and allows async stores; the
   server pays one `block_on` per round (today: one per fee).
6. **Hand-written `match` in the server's language enums** (three per enum,
   one arm per variant) instead of a macro. *Recommend:* yes. It is about
   30 lines in all, and a reader sees each delegation.
7. **Fix B1, B2, B5, B6 as listed?** *Recommend:* yes, each as its own step
   after step 4.
8. **Should the `Round` worker itself be generic in the package?**
   *Recommend:* no. Its topic, `Skip` policy and job rows tie it to the
   server's broadcasts.

## 9. As built: where the code differs (`sdk/trigger`)

- Errors are one `error::Error { class, message }` (section 2), not one
  enum per domain; the selection's two "no wallet" texts stay distinct.
  `UnknownAction` was to map to 404, as asked in review; the server keeps
  today's 400 (an unknown action name is a malformed body, not a missing
  resource), so the switch changes no status.
- The read-once cache is `CachedStore` in `store.rs`, a crate-private
  `Store` over the caller's, keyed by selector and by (address, asset) pairs, for the
  whole round. `Context` itself reads straight through, so one used
  outside a round (a test) is uncached.
- `Context::new(wallets, fees, assets, network, now, last_run)` starts
  with an empty `this`; `with` sets it. `Context::select(selector)` is
  public too.
- `Selection` no longer carries the confirmed position: the store reads at
  it, and the round records it in `LastRound`. `Index::held` returns the
  decoded balance (no decode error). `Index::new` builds today's id.
- `Trigger::decide(actions, on, stats) -> Vec<E>` fills `round::Stats` as
  it goes; the round sets `jobs = effects.len()`.
- `Trigger::validate` checks the action first, then selector, filter and
  check: the API's order today. The filter's "reads this" rule runs after
  the filter's own rules, over every condition in it (`Logic::reads_this`
  is "every one does"); a filter that breaks both shows the other rule
  first.
- `LastRound.at` and `confirmed` are `u64` (the JSON numbers are the same).
- The leaf variants are `Logic::One` and `Arithmetic::Term`. Both serialize
  by hand so no `Clone` bound is needed; the JSON is today's.
- `utoipa` is a workspace dependency (`=5.5.0`, the server's version).
- R1 ("confirmed, not tip") stays the store's contract: the package can
  only test that the round records the position it was given.
