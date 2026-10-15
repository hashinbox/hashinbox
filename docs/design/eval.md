# Pipes: evaluation

Status: draft, step by step. Extends `065-pipes.md`.

How one evaluation of one trigger runs, one step at a time. Each step says what
it reads, what it produces, and how to build and test it.

## Step 1: Select

Turns the trigger's `selector` into the set of wallet indexes the rest of the
evaluation works on (`This`).

### Input

- The trigger: its (chain, network) and its `selector`.
- The network's confirmed tip position, read once per evaluation.
- Every other enabled trigger of the network (to enforce one trigger per
  wallet index).

### Output

```rust
/// The wallet indexes a selector picked, ordered by id.
pub(crate) struct Selection(BTreeSet<WalletIndexId>);   // id: chain.network.address.asset
```

### How it resolves

One SQL query, built with diesel. Every selector leaf is a condition on a
single `wallet_index` row (none compares two rows), so the set operations of a
selector are boolean logic in one `WHERE`: `All` is `AND`, `Any` is `OR`, `Not`
is `NOT`. Only the selected rows are loaded.

| Selector | Condition on the row |
| --- | --- |
| `Ids(ids)` | `id IN (…)` (unknown ids match nothing) |
| `Asset(a)` | `asset = a` |
| `Tag(t)` | `EXISTS (SELECT 1 FROM tag WHERE tag.chain = wallet_index.chain AND tag.network = wallet_index.network AND tag.address = wallet_index.address AND tag.tag = t)` |
| `All(xs)` | each `x` joined with `AND` (empty: true) |
| `Any(xs)` | each `x` joined with `OR` (empty: false) |
| `Not(x)` | `NOT x` |

The rest of the `WHERE` is the **universe**: the trigger's network, and only
**fully indexed** indexes (indexed through a live block at or above the
confirmed tip). `Not` is therefore relative to the universe without extra work.

```rust
type Predicate = Box<dyn BoxableExpression<wallet_index::table, Sqlite, SqlType = Bool>>;

/// The selector as one boolean expression over a `wallet_index` row.
fn predicate(selector: &Selector) -> Predicate {
    match selector {
        Selector::Ids(ids) => Box::new(wallet_index::id.eq_any(ids.clone())),
        Selector::Asset(asset) => Box::new(wallet_index::asset.eq(asset.id.clone())),
        Selector::Tag(name) => Box::new(exists(tag::table.filter(
            tag::chain.eq(wallet_index::chain)
                .and(tag::network.eq(wallet_index::network))
                .and(tag::address.eq(wallet_index::address))
                .and(tag::tag.eq(name.clone())),
        ))),
        Selector::All(all) => all.iter().map(predicate)
            .reduce(|a, b| Box::new(a.and(b))).unwrap_or_else(|| Box::new(true.into_sql::<Bool>())),
        Selector::Any(any) => any.iter().map(predicate)
            .reduce(|a, b| Box::new(a.or(b))).unwrap_or_else(|| Box::new(false.into_sql::<Bool>())),
        Selector::Not(not) => Box::new(diesel::dsl::not(predicate(not))),
    }
}

/// The trigger's selection: the universe filtered by its selector.
fn select(connection, trigger: &Trigger, confirmed: u64) -> QueryResult<Vec<WalletIndexRow>> {
    wallet_index::table
        .inner_join(block::table.on(block::id.nullable().eq(wallet_index::indexed_at)))
        .filter(wallet_index::chain.eq(&trigger.chain).and(wallet_index::network.eq(&trigger.network)))
        .filter(block::archived_at.is_null().and(block::position.ge(confirmed)))
        .filter(predicate(&trigger.selector))
        .select(WalletIndexRow::as_select())
        .order(wallet_index::id)
        .load(connection)
}
```

- An index never indexed (`indexed_at` NULL) is dropped by the inner join; one
  behind the confirmed tip or indexed through an archived block, by the block
  filter. So no selector can pick it.
- If diesel refuses the outer `wallet_index` column inside `exists`, `Tag` uses
  the equivalent `wallet_index::address.eq_any(tag::table.filter(…).select(tag::address))`.

**Contested indexes** (one trigger per wallet index): one query per evaluation,
the same universe filtered by `predicate(this) AND (predicate(other₁) OR …)`
over the network's other enabled triggers. Rows it returns are dropped from the
selection and reported (logged and shown on the trigger). Saving a trigger and
changing tags refuse overlaps, so this only happens through state that changed
under them (a new asset indexed).

### Why SQL, not memory

- Memory then holds only the selected rows, not every wallet index of the
  network (about 300 bytes each: 300 MB at a million indexes, reloaded on every
  evaluation).
- SQLite's planner uses the `wallet_index` and `tag` indexes; the work grows with
  what a trigger selects, not with the number of wallets.
- Diesel builds the query from the selector tree with typed, boxed expressions,
  so it is checked at compile time and parameterized (no SQL strings).

### Build

1. `models/tag.rs` (from 065's plan): `tag` table and its queries.
2. `models/wallet_index.rs`: `select(trigger, confirmed)` and the contested
   query, with `predicate(&Selector)` beside them (the SQL lives in the model).
3. `pipes/select.rs`: the Select step: reads the confirmed tip, calls the model,
   drops contested indexes and reports them, returns the `Selection`.

### Test

- `Ids`, `Asset`, `Tag` each pick exactly their indexes; unknown ids pick
  nothing.
- `All`, `Any`, `Not` match set intersection, union and difference; `Not` is
  relative to the universe.
- Another network's indexes are never picked.
- An index with `indexed_at` NULL, behind the confirmed tip, or indexed through
  an archived block is not picked; it is picked once indexed to the tip.
- A tag added to an address brings its indexes in at the next evaluation.
- An index selected by two triggers is dropped from both and reported.

## Step 2: Check

Decides whether the trigger acts now: evaluates the trigger's `check` once over
`This`. The same evaluator later serves Filter (one index as `This`) and the
Action's amounts, through one per-evaluation cache.

### Input

- `This`: the `Selection` from Step 1 (after Filter, once it exists).
- The confirmed tip position, the one Step 1 read (never re-read: every term
  sees the same tip).
- The trigger's `check`, and its (chain, network).
- The run record, only as one read: the trigger's latest run, when it started
  and whether it is settled (Idle, Since below). The table is a later step.
- `now` (Unix ms), passed in, so `Since` is testable.
- Reads: the SQLite connection (balances, via the models) and the network's
  fee estimator (Fee below).
- Asset decimals: the network's `base::AssetInfo` (what `models::Asset`
  carries as `decimals`), by asset id.

### Output

```rust
/// Whether the check passed, and every figure read to decide it.
pub(crate) struct Verdict {
    pub passed: bool,
    pub reads: Reads,
}

/// One evaluation's reads, each made at most once. Filter and Action take it
/// over, so a balance or fee the check read is the one they use.
#[derive(Default)]
pub(crate) struct Reads {
    /// Confirmed atomic balance per wallet index id.
    balances: BTreeMap<WalletIndexId, Decimal>,
    /// Target resolutions: (selection, asset override or selector) to ids;
    /// `None`: some index is not fully indexed (the term is unknown).
    targets: BTreeMap<TargetKey, Option<Selection>>,
    /// Native atomic fee per (address, asset).
    fees: BTreeMap<(String, String), Decimal>,
    /// The latest run, read once.
    run: Option<Option<LastRun>>,
}
```

Decision: **cache**. Use case 4 reads `Fee { of: This { asset: t } }` in the
filter and the action, and use case 1 reads `Fee` in the check and the action;
a fee is an RPC call, and two reads of it can differ between calls, which would
make the check and the action disagree. One read per evaluation keeps them
consistent. The cache lives for one evaluation only; nothing is kept between
evaluations.

A read error (SQL, RPC, decoding) is an **error**, not `false`: the evaluation
stops, nothing is sent, and it is reported and retried at the next evaluation.
Otherwise `Not` over a failed read would pass.

### How it evaluates

#### Resolving a `Target`

| Target | Wallet index ids |
| --- | --- |
| `This {}` | the selection |
| `This { asset: x }` | for each selected id `chain.network.address.a`: `chain.network.address.x` |
| `Select(sel)` | Step 1's `predicate(sel)` over the same universe (network, fully indexed), without the contested-index pass (a read is not ownership) |

`This { asset: x }` is built in Rust from the natural keys (ids are
`chain.network.address.asset`), then confirmed in one query: the same universe
filter as Step 1 over `id IN (…)`.

Decision: **the counterparts must be fully indexed too**. A missing or
backfilling index would read as 0. In use case 4 ("fund"), a deposit whose ETH
index is still backfilling would read 0 ETH, look short of gas and be funded
again. So when any counterpart is missing from the confirmed query, the term
is **unknown** and the `Compare` using it fails (in a check: the check fails;
in Filter: that one index is dropped). Fail closed, reported, never 0.

#### Balances, in SQL and Rust

The schema keeps no running total (`sdk.sql`: amounts are exact decimal TEXT,
"never summed in SQL"; `history.rs` folds `LegRow::find_live` with
`LegFacts::signed`). SQLite cannot sum u256 TEXT exactly. Decision: **load the
legs, fold in Rust**, one query per target for all its indexes:

```sql
SELECT wi.id, l.kind, l.amount
FROM wallet_index wi
JOIN leg l   ON l.address = wi.address AND l.asset = wi.asset
JOIN block b ON b.id = l.block AND b.chain = wi.chain AND b.network = wi.network
WHERE wi.id IN (SELECT value FROM json_each(?1))
  AND b.archived_at IS NULL
  AND b.position <= ?2          -- the confirmed tip
```

Folded per `wi.id` with `LegFacts::signed` (received, minted add; spent,
burned, fee subtract; rewards are signed) into `Decimal`. Only ids not already
in `Reads.balances` are queried; an id with no legs is 0. It is served by
`leg_address (address, asset)` and the block primary key.

- Cost grows with each index's leg count, as `/balance` does today. If that
  becomes the bottleneck, the cheaper exact answer is a running total kept on
  the leg rows at commit (as 057 described): a schema and indexer change,
  proposed separately, not part of this step.
- A negative confirmed balance (should not happen) is an error, not a value.

#### Compare

1. Resolve `of`; unknown → the compare fails.
2. Aggregate over its indexes' balances (atomic):
   - `Sum`: the sum; empty → 0.
   - `Min`, `Max`: empty → no value, the compare fails.
   - `Count`: the number of indexes, balances not read; empty → 0.
3. Evaluate `value` (an `Amount`, below) to atomic units.
4. Apply the operator, exact `Decimal`: `Above` `>`, `AtLeast` `>=`, `Below`
   `<`, `AtMost` `<=`, `Equal` `==`, `Around { tolerance }`:
   `|aggregate − value| <= tolerance` (tolerance in display units, scaled like a
   `Value`).

**Units.** All arithmetic is in atomic units. A `Value(d)` or a tolerance is
scaled once: `d × 10^decimals` of the compare's asset; it must be exact (more
fractional digits than `decimals` is refused when the trigger is saved). For
`Count`, `Value(n)` is a plain whole number, not scaled, and only `Value` is
allowed.

**Amount evaluation** (atomic, exact; `Sub` may go below zero, which the
operator then sees):

- `Balance { of }`: the `Sum` of `of`; unknown → unknown.
- `Fee { of }`: below. It is always in the network's **native** asset (use case
  4 compares an ETH balance with the fee of sending a token).
- `Value(d)`: scaled with the asset of the expression it sits in.
- `Add`, `Sub`, `Min`, `Max`: checked `Decimal` operations; unknown
  propagates.
- Every term of one `Amount` and the compared aggregate must be in one asset
  (`Balance` in its target's asset, `Fee` in native); a mix is refused when the
  trigger is saved. This clarifies 065, which says "Fee … in its asset".

#### Fee

`Fee { of }` = the network's **fee estimator** (`base::FeeEstimator`, from the
network's factory, `WalletFactory::fees()`) asked about the addresses of `of`:

```rust
let sources: Vec<(Address, Asset)> = indexes.iter().map(|i| (i.address(), i.asset())).collect();
let fee = context.fees.estimate(&sources).await?;   // native atomic, worst case
```

- One call for the whole target, not one per address: the estimator prices the
  send the action will make. On a UTXO chain all sources are one transaction
  (one overhead, one output); on an account chain one transaction per sender,
  summed.
- It is computed from the estimator's cached network prices (refreshed on a
  TTL), so it costs arithmetic plus, on Bitcoin, one store read of the sources'
  coins. No signing, no key: watch-only addresses can be priced.
- It is worst case: a send funded by it goes through. Ethereum token gas is
  estimated once per token toward a placeholder non-holder (the dearest case);
  Solana includes the rent of creating the recipient's token account when the
  asset is a token.
- **Check and action agree.** The builder that signs the send uses the same
  estimator instance (handed out by the factory), and `Reads` caches the value
  per evaluation, so `Sub(Balance, Fee)` decided here is exactly what the
  builder pays.

#### Idle and Since

They read one thing, the trigger's latest run:

```rust
/// What the check needs from the run record.
pub(crate) struct LastRun {
    /// When it started (Unix ms).
    pub started_at: i64,
    /// Every send of it is confirmed or definitively failed.
    pub settled: bool,
}
```

- `Idle`: no run, or the latest is settled.
- `Since { seconds }`: no run, or `now − started_at >= seconds × 1000`.
- Read at most once per evaluation (`Reads.run`). Its table and what
  "settled" is derived from are the run-record step.

#### All, Any, Not

- `All`: in written order, stop at the first that fails; empty passes.
- `Any`: in written order, stop at the first that passes; empty fails.
- `Not`: the opposite; an error stays an error.
- Order is the author's: write `Idle` first so a busy trigger reads nothing
  else. No reordering, so what is read is predictable.
- Repeated terms read once through `Reads`.

```rust
impl<L> Logic<L> {
    /// Evaluates in written order, stopping early; `leaf` decides each leaf.
    fn eval(&self, leaf: &mut impl FnMut(&L) -> anyhow::Result<bool>) -> anyhow::Result<bool> {
        Ok(match self {
            Logic::Leaf(l) => leaf(l)?,
            Logic::All(all) => { for x in all { if !x.eval(leaf)? { return Ok(false) } } true }
            Logic::Any(any) => { for x in any { if x.eval(leaf)? { return Ok(true) } } false }
            Logic::Not(not) => !not.eval(leaf)?,
        })
    }
}

/// One comparison over `this`.
fn compare(c: &Compare, this: &Selection, context: &mut Context) -> anyhow::Result<bool> {
    let Some(indexes) = context.resolve(&c.of, this)? else { return Ok(false) };
    let Some(left) = context.aggregate(&indexes, &c.aggregate)? else { return Ok(false) };
    let Some(right) = context.amount(&c.value, this, c.of.asset())? else { return Ok(false) };
    Ok(c.operator.holds(&left, &right))
}

/// The trigger's check: comparisons plus its own run state.
fn check(check: &Logic<Gate>, this: &Selection, context: &mut Context) -> anyhow::Result<bool> {
    check.eval(&mut |gate| match gate {
        Gate::Compare(c) => compare(c, this, context),
        Gate::Idle => Ok(context.last_run()?.is_none_or(|run| run.settled)),
        Gate::Since { seconds } => Ok(context.last_run()?
            .is_none_or(|run| context.now - run.started_at >= i64::try_from(*seconds)? * 1000)),
    })
}

/// The trigger's filter for one index: comparisons only (the type has no run state).
fn filter(filter: &Logic<Compare>, index: &Selection, context: &mut Context) -> anyhow::Result<bool> {
    filter.eval(&mut |c| compare(c, index, context))
}
```

`Context` holds the connection, the confirmed tip, `now`, the fee estimator,
the asset decimals and `Reads`; `resolve`, `aggregate`, `amount` and
`last_run` fill `Reads` on first use.

### Why

- One query per target, folded in Rust: exact for u256, no float, no SQL sum,
  and the same rule `/balance` uses, limited to the confirmed tip.
- Confirmed tip and fully indexed counterparts: no balance from history that
  can change or is incomplete; unknown fails closed.
- Cached reads: the check, filter and action decide on the same figures.
- Errors are not `false`: `Not` cannot turn a failed read into a send.

### Build

1. `models/leg.rs`: `confirmed_balances(connection, ids, confirmed)`, the query
   above, folded with `LegFacts::signed` into `BTreeMap<id, Decimal>`.
2. `models/wallet_index.rs`: `fully_indexed(connection, ids, confirmed)` (the
   Step 1 universe over `id IN …`), and `select` reused for `Select` targets.
3. Uses `base::FeeEstimator` (`WalletFactory::fees()`); nothing new in base.
4. `pipes/check.rs`: `Context`, `Reads`, `Verdict`; target resolution,
   aggregates, operators, amount evaluation, `Idle`, `Since`,
   `All`/`Any`/`Not`. The run read is a parameter (a closure or the run model)
   until the run-record step.

### Test

- Balance: received + minted + rewards − spent − burned − fee, per index, in
  one query for several indexes; legs above the confirmed tip and in archived
  blocks are ignored; a u256 amount above `i64` sums exactly.
- `This { asset: x }`: reads the same addresses in x; a counterpart missing,
  never indexed, behind the tip or through an archived block makes the compare
  fail (and drops only that index in Filter).
- `Select(sel)` target reads the indexes Step 1 would pick, without the
  contested pass.
- Aggregates: `Sum`/`Count` of empty is 0; `Min`/`Max` of empty fail; `Count`
  ignores balances.
- Operators at the boundary: `Above` vs `AtLeast`, `Below` vs `AtMost`,
  `Equal`; `Around` just inside and just outside the tolerance.
- Units: `Value(5)` BTC is 500 000 000 sats; `Value(0.0000000001)` BTC is
  refused at save; `Count` compares with a whole number.
- Amount: `Sub(Balance, Fee)` below zero compares as negative; `Max(…, 0)`
  clamps; mixed assets refused at save.
- Fee (with test doubles of each chain's RPC): Bitcoin all coins to one
  output, no change; Ethereum token transfer while holding 0 ETH; Solana fee
  for the message; read once when used in check and action.
- `Idle`: no run passes; unsettled run fails; settled passes. `Since`: just
  before and at `seconds`.
- `All` stops at the first failure and `Any` at the first success (a counting
  fee double sees no call); `Not` of an error is an error.
- A read error fails the evaluation and reports it; nothing reaches Action.

## Next steps

- **Filter**: run `filter` (a `Logic<Compare>`) per selected index with
  `This` = that index; drop failures and unknowns; share `Reads`. `Idle` and
  `Since` cannot appear (the leaf type is `Compare`).
- **Action**: send shapes (many to one, one to many), `Amount` per source or
  destination from `Reads`, skip ≤ 0, keyed sources only, sends into the queue.
- **Commit**: the run record (`LastRun` and what settles it) written with the
  run's sends atomically.
