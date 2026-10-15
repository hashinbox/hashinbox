# 065: Pipes

Status: draft.

A pipe moves funds automatically when indexing changes a wallet. It is stored
as a **trigger**: a **selector** (which wallet indexes it covers), a **check**
(whether to act now) and an **action** (a send).

## Use cases

Each use case states what must happen and what must never happen.

### 1. ETH forward

When address A receives ETH, send A's ETH to wallet C.

- Forward everything A holds, less the fee.
- Never leave A unable to pay the fee for the forward itself.
- Dust below a set minimum is not forwarded.

### 2. BTC forward

When address A receives BTC, send A's BTC to wallet C.

- Spend all of A's coins in one transaction, less the fee.
- Dust below a set minimum is not forwarded.

### 3. BTC consolidation over a threshold

Addresses A1..An receive BTC. When their summed balance reaches 5 BTC, gather
their funds into wallet C.

- The trigger is the sum across the set, not any single address.
- After a successful gather the sum is below the threshold, so nothing fires
  again until it is crossed again.
- If a send fails, the next evaluation retries while the sum is still at or
  over the threshold.

### 4. ERC-20 sweep with gas funding

When address A receives token T, send A's T to wallet C (or B). A holds no
ETH, so wallet B funds A's gas first.

- Fund A from B only when A lacks gas, and only by what is needed (plus
  headroom).
- Send T only when A has gas.
- If funding was not enough, funding happens again; the sweep never gets stuck
  half done.

## Principles

- **One procedure per evaluation.** When a trigger is evaluated it runs once:
  resolve its selector into a set, narrow it with its filter, evaluate its check
  once, and if it passes run its action once. Nothing is run per wallet as a
  separate job; per-wallet decisions are the filter.
- **Level-triggered, not multistep.** Every evaluation looks at current state
  and decides from it alone. There is no stored step or workflow; the next
  evaluation looks again. A multistep goal (use case 4) is several independent
  triggers, each correct on its own, whose checks are exact opposites.
- **No double send.** A trigger does not act again while its previous run is
  unfinished or not yet confirmed (`Idle`).
- **Confirmed funds.** Balances are confirmed balances, so a reorg cannot make a
  trigger send funds it does not have; and only wallet indexes indexed up to the
  confirmed tip take part, so no balance is read from incomplete history.
- **One trigger per wallet.** A wallet index is covered by at most one trigger,
  so two triggers never fight over the same funds.

## Design

### Tags

A wallet carries any number of tags: short labels such as `hot`, `deposit`,
`sweep-usdc`. Tags exist only to select wallets; they mean nothing else.

- Tags belong to an address of a network (`chain.network.address`), keyed or
  watch-only alike, so a watch-only deposit address can be tagged too.
- A tag is lowercase `[a-z0-9-]`, 1 to 64 characters (no dots, so it fits the
  dotted ids).
- Stored one row per (address, tag): table `tag`, id
  `{chain}.{network}.{address}.{tag}`.
- Added and removed through the wallet API; a wallet's tags are listed with it.

### Selector

A selector picks a set of **wallet indexes** (one address in one asset) of the
trigger's network.

```rust
pub(crate) enum Selector {
    /// These wallet indexes, by id (`chain.network.address.asset`).
    Ids(Vec<String>),
    /// Every wallet index of this asset.
    Asset(Asset),
    /// Every wallet index whose address carries this tag.
    Tag(String),
    /// In every one of these selectors (AND).
    All(Vec<Selector>),
    /// In any of these selectors (OR).
    Any(Vec<Selector>),
    /// Not in this selector.
    Not(Box<Selector>),
}
```

A selector is **absolute**: it picks from the network's wallet indexes. What a
filter, check or action refers to is a **target**: a selector, or **`This`**,
the wallet indexes in context.

```rust
/// The wallet indexes in context: the trigger's (filtered) selection, or while
/// filtering, the one index being tested.
pub(crate) struct This {
    /// Read them in another asset of the same addresses (e.g. their ETH);
    /// `None`: as they are.
    pub asset: Option<Asset>,
}

/// What a balance, fee, compare or send refers to.
pub(crate) enum Target {
    This(This),
    Select(Selector),
}
```

- Composable: `All`, `Any` and `Not` combine the leaves freely.
- **A trigger is per network.** It names one (chain, network) and its selector
  picks only that network's wallet indexes, whatever its leaves say; a pipe
  across networks is several triggers.
- **Only fully indexed wallet indexes are selected.** An index whose
  `indexed_at` is below the network's confirmed tip (still backfilling from its
  birthday, or behind) is left out: its balance would be incomplete, so a check
  or an amount could be wrong. It joins automatically once indexing reaches the
  confirmed tip.
- A trigger's own selector is a `Selector`, so it cannot refer to `This` (the
  type rules it out). It needs at least one positive leaf (`Ids`, `Asset` or
  `Tag`) outside a `Not`; validated when the trigger is saved.
- A selector is resolved against current state at each evaluation: tagging a
  wallet, or indexing a new asset of it, brings it in without editing the
  trigger.
- **One trigger per wallet.** When a trigger is saved, its selector may not pick
  a wallet index another trigger picks. A tag change that would make a wallet
  index match two triggers is refused; a wallet index that still matches two
  (e.g. a new asset indexed) is skipped at evaluation and reported.

Examples:

- One address: `Ids(["ethereum.mainnet.0xa….native"])`.
- Every tagged USDC deposit address: `All([Tag("deposit"), Asset(usdc)])`.
- Every BTC address except the cold wallet: `All([Asset(btc), Not(Tag("cold"))])`.
- Targets: `This {}` (the wallet indexes in context, as they are),
  `This { asset: eth }` (the same addresses, read in ETH),
  `Select(Ids([b_eth]))` (an absolute choice).

### Check

A check decides whether the trigger acts now. It is evaluated once per
evaluation over the filtered set. The trigger's **filter** uses the same
combinators over balance comparisons only, evaluated once per wallet index (see
Filter).

The combinators are written once, generic over what they combine:

```rust
/// AND / OR / NOT over leaves of one kind.
pub(crate) enum Logic<L> {
    /// One leaf.
    Leaf(L),
    /// Every one passes (AND).
    All(Vec<Logic<L>>),
    /// Any passes (OR).
    Any(Vec<Logic<L>>),
    /// The opposite (NOT).
    Not(Box<Logic<L>>),
}

/// `aggregate` of the balances of `of`, compared with `value`, an [`Amount`]
/// evaluated at the same time.
pub(crate) struct Compare {
    pub of: Target,
    pub aggregate: Aggregate,
    pub operator: Operator,
    pub value: Amount,
}

/// What a trigger's check may test: balances, and the trigger's own runs.
pub(crate) enum Gate {
    /// A balance comparison over the (filtered) selection.
    Compare(Compare),
    /// No earlier run of this trigger is in flight: queued, broadcast, or
    /// mined but not yet confirmed.
    Idle,
    /// At least `seconds` since this trigger's last run.
    Since { seconds: u64 },
}

// On the trigger:
//   filter: Option<Logic<Compare>>   per wallet index: comparisons only
//   check:  Logic<Gate>              the whole trigger: comparisons and run state

/// Many wallet indexes' balances into one number.
pub(crate) enum Aggregate {
    /// Their summed balance.
    Sum,
    /// The smallest balance.
    Min,
    /// The largest balance.
    Max,
    /// How many there are (balance ignored).
    Count,
}

/// How the aggregate compares with the value.
pub(crate) enum Operator {
    /// Strictly greater.
    Above,
    /// Greater or equal.
    AtLeast,
    /// Strictly less.
    Below,
    /// Less or equal.
    AtMost,
    /// Exactly equal.
    Equal,
    /// Within `tolerance` of the value, either side.
    Around { tolerance: Decimal },
}
```

- **Balance** is the confirmed balance of a wallet index (live legs in blocks
  at or below the network's confirmed tip).
- **Value** is an `Amount`: `Value(5)` for a fixed number in the asset's
  display units (`5` BTC, `0.002` ETH), or a live term such as
  `Fee { of: This {} }`, so a check and an action compare against the same
  computed figure. For `Count` it is a plain number.
- **Limits are checks.** Cooldowns and waits are `Idle` and `Since`, combined
  with the rest: `All([Idle, Since { 60 }, Compare(…)])`. The evaluator has no
  separate limits.
- **One asset per compare**: `Sum`, `Min` and `Max` over different assets are
  meaningless, so a `Compare` whose target can span more than one asset is
  refused when the trigger is saved (a `Select` needs an `Asset` leaf; `This`
  is one asset, or the one it names). `Count` may span assets.
- **Empty selection**: `Sum` and `Count` are 0; `Min` and `Max` have no value,
  so the compare fails.

### Filter

A trigger's optional filter narrows its selection one wallet index at a time:
each selected index is kept when the filter (a `Logic<Compare>`) passes with
`This` being that one index. Afterwards `This` is the narrowed set, for the check
and the action. It is how a per-wallet decision is made inside the one
procedure.

- Example: of the deposit token indexes, keep those whose address holds the
  gas for that token:
  `Compare { of: This { asset: eth }, Sum, AtLeast, Fee { of: This {} } }`.
- One filter per trigger. "Send from wallets that pass X to wallets that pass
  Y" is two triggers.
- `Idle` and `Since` are about the trigger, not one index; the filter's leaf
  type is `Compare`, so it cannot contain them.

### Action

What the trigger does when its check passes. Send is the only action now; more
kinds (an email or webhook notification, …) are new variants later.

```rust
pub(crate) enum Action {
    Send(Send),
}

/// Moves `amount` of `asset` from the `source` wallet indexes to the
/// `destination` addresses.
pub(crate) struct Send {
    /// Where the funds come from.
    pub source: Target,
    /// What is sent; every source is read in this asset.
    pub asset: Asset,
    /// Where the funds go.
    pub destination: Target,
    /// How much, per send.
    pub amount: Amount,
}
```

- **Shape**: at most one side may be many.
  - Many sources, one destination: each source pays the destination (use cases
    1 to 3). `amount` is evaluated per source, with `This` being it.
  - One source, many destinations: the source pays each destination (use case
    4, funding). `amount` is evaluated per destination, with `This` being it.
  - Many to many is refused at each run (and when it is evident at save).
- **Many sources on a UTXO chain** should be one transaction spending all their
  inputs; until the builder can spend several wallets' coins (deferred), it is
  one send per source. On an account chain (ETH, SOL) one transaction has one
  sender, so it is always one send per source.
- Every source must be keyed here (a `wallet` row); a watch-only source is
  refused when the trigger is saved and again when it runs.
- All sends of one run go to the send queue together, as one run of the
  trigger (one lane per sender, signed bytes saved before broadcast).

### Amount

An expression over a few terms, so new forms need no new code paths.

```rust
pub(crate) enum Amount {
    /// The summed confirmed balance of `of`, in its asset.
    Balance { of: Target },
    /// The network fee of sending everything `of` holds, in its asset, to one
    /// address.
    Fee { of: Target },
    /// A fixed value, in the asset's display units.
    Value(Decimal),
    Add(Box<Amount>, Box<Amount>),
    Sub(Box<Amount>, Box<Amount>),
    Min(Box<Amount>, Box<Amount>),
    Max(Box<Amount>, Box<Amount>),
}
```

| Intent | Amount |
| --- | --- |
| Everything (token: fee paid in native) | `Balance { of: This {} }` |
| Everything less the fee (native) | `Sub(Balance { of: This {} }, Fee { of: This {} })` |
| Everything but a reserve X | `Sub(Balance { of: This {} }, Value(X))` |
| Gas shortfall, never negative | `Max(Sub(Fee { of: This { asset: token } }, Balance { of: This {} }), Value(0))` |
| At most X | `Min(Balance { of: This {} }, Value(X))` |

- **`Fee` is computed at evaluation from current state**: the chain's fee for
  sending all of the asset from those addresses to one output: on Bitcoin the
  rate times the size of a transaction spending every coin to one output; on
  Ethereum the worst case `gas_limit * max_fee` (gas estimated for that
  transfer); on Solana the node's fee for that message. It needs no amount, so
  `Sub(Balance, Fee)` is exact: given that amount the builder spends every
  coin, has nothing left for change, and pays exactly that fee.
- A result of zero or less means "nothing to send": that send is skipped, not
  failed.
- Values are in display units; the result is converted to atomic units with
  the asset's decimals, and must fit them exactly.

### Use cases as triggers

Short forms below: `This` is `This {}`; `Balance` and `Fee` are `Balance { of: This {} }`
and `Fee { of: This {} }`; `Ids(…)` as a target is `Select(Ids(…))`.

1. **ETH forward** (A to C):
   - selector: `Ids([a_eth])`
   - check: `All([Idle, Compare { of: This, Sum, Above, Fee }])`
   - action: `Send { source: This, asset: eth, destination: Ids([c]), amount: Sub(Balance, Fee) }`
2. **BTC forward**: the same with BTC; `Fee` covers spending all of A's coins.
3. **BTC consolidation at 5 BTC** (tag `collect` to C):
   - selector: `All([Tag("collect"), Asset(btc)])`
   - filter: `Compare { of: This, Sum, Above, Fee }` (only members worth spending)
   - check: `All([Idle, Compare { of: This, Sum, AtLeast, Value(5) }])`
   - action: `Send { source: This, asset: btc, destination: Ids([c]), amount: Sub(Balance, Fee) }`

   One run, one decision; every member with funds above its fee sends.
4. **ERC-20 sweep with gas funding** (tag `deposit`, token T, gas from B, to C):
   two triggers on disjoint wallet indexes, as "one trigger per wallet" requires.
   - **fund** (the deposits' ETH indexes):
     - selector: `All([Tag("deposit"), Asset(eth)])`
     - filter: `All([Compare { of: This { asset: t }, Sum, Above, Value(0) },
       Compare { of: This, Sum, Below, Fee { of: This { asset: t } } }])`
     - check: `Idle`
     - action: `Send { source: Ids([b_eth]), asset: eth, destination: This,
       amount: Max(Sub(Fee { of: This { asset: t } }, Balance), Value(0)) }`
   - **sweep** (the deposits' T indexes):
     - selector: `All([Tag("deposit"), Asset(t)])`
     - filter: `Compare { of: This { asset: eth }, Sum, AtLeast, Fee }`
     - check: `Idle`
     - action: `Send { source: This, asset: t, destination: Ids([c]), amount: Balance }`

   For any one address, "ETH below the token's fee" and "ETH at least the
   token's fee" are exact opposites, so it is funded or swept, never stuck.

### Persistence: the `trigger` table

A trigger is one row: its selector, filter, check and action serialized as JSON, so
triggers are created, listed, changed and loaded without code. The Rust types
above are the only format; they derive `Serialize` and `Deserialize`, so the
JSON reads like the types.

```sql
-- One trigger. id: its slug ([a-z0-9-], no dots), chosen at creation.
CREATE TABLE "trigger" (
    id         TEXT    NOT NULL PRIMARY KEY,
    chain      TEXT    NOT NULL,
    network    TEXT    NOT NULL,
    selector   TEXT    NOT NULL CHECK (json_valid(selector)),
    filter     TEXT             CHECK (json_valid(filter)), -- NULL: no filter.
    "check"    TEXT    NOT NULL CHECK (json_valid("check")),
    action     TEXT    NOT NULL CHECK (json_valid(action)),
    enabled    INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    created_at INTEGER NOT NULL, -- Unix ms.
    updated_at INTEGER NOT NULL  -- Unix ms.
);
```

Example row (use case 4, sweep):

```json
{
  "id": "sweep-usdc",
  "chain": "ethereum", "network": "mainnet",
  "selector": {"all": [{"tag": "deposit"}, {"asset": "0xa0b8…eb48"}]},
  "filter": {"compare": {
    "of": {"this": {"asset": "native"}},
    "aggregate": "sum", "operator": "at_least",
    "value": {"fee": {"of": {"this": {}}}}}},
  "check": "idle",
  "action": {"send": {
    "source": {"this": {}},
    "asset": "0xa0b8…eb48",
    "destination": {"select": {"ids": ["ethereum.mainnet.0xc0ff….native"]}},
    "amount": {"balance": {}}}},
  "enabled": true
}
```

- **Loaded at start and on change**: the evaluator reads every enabled trigger
  of its network; the API's writes notify it, so a change applies at the next
  evaluation without a restart.
- **Validated before it is stored**: the JSON must deserialize into the types
  and pass the rules above. A row that fails to load later (hand-edited)
  disables that trigger and is reported, never skipped silently.
- **Amounts are decimal strings**, never JSON numbers, so no float touches them.
- **Changing a trigger** replaces its JSON; a run already queued keeps the sends
  it was decided with.

### Deferred

1. **One transaction from many keys** (use case 3 as a single transaction)
   is now supported: a builder takes each transfer's source via
   `Transfer::from`, and a Bitcoin builder spends every source's coins in one
   transaction, each input signed by its own key.

### To be written

Evaluation loop (when triggers are evaluated), the run record behind `Idle` and
`Since`, API, and safety (amount caps, global switch).

## Plan

1. Wallet tags: `tag` table, model, storage, wallet API to add, remove and list
   tags; refuse a tag change that makes a wallet index match two triggers.
2. Selector (absolute: leaves, `All`/`Any`/`Not`) and Target (`This { asset }`
   or a selector); validation; resolution against wallet indexes and tags. Filter: per-index `Logic<Compare>` that
   narrows the selection.
3. Check: `Logic<L>` (one `All`/`Any`/`Not`), `Compare` (target, aggregate,
   operator, `Amount` value), `Gate` (`Compare`, `Idle`, `Since`);
   confirmed-balance reads per wallet index.
4. Action and Amount: send shapes (many to one, one to many), amount evaluation
   with `Fee` computed per chain from current state.
5. `trigger` table (with `filter`), model and storage; serde forms; validation on save
   (including one trigger per wallet); load on start and on change.
6. (Next sections, once designed.)
