# 077: `sdk/trigger`: the trigger engine as a package

Status: design, no code changed. Paths under `apps/server/` unless noted.

The trigger engine (definitions, validation, evaluation, the `send` plan)
lives in the server today, spread over 3 models, a usecase, a worker and an
action registry (~2,000 lines without tests). This design moves everything
that does not need SQL, HTTP or the job queue into a new package,
`sdk/trigger`. The server keeps storage, HTTP and job wiring, the same way
`packages/tasks` and `sdk/base` already split the work: the package declares
what it reads, and the server implements that over SQLite in `usecase`.

## Decisions first

1. **New crate `trigger` at `sdk/trigger`**, depending on `base`, `serde`,
   `anyhow` and nothing else (optional `utoipa` behind feature `openapi`).
   It never touches SQL, `tasks`, `payments` or tokio.
2. **One read port, `Snapshot`**: everything a round reads, from one
   consistent view of the store: the confirmed position, the enabled triggers
   with their latest run, and wallet indexes picked by a selector or named by
   (address, asset). Async, `base::Future`, `anyhow`, as `base`'s stores are.
3. **No write port.** A round returns a value (`Round`): each trigger's
   `LastRound` and the sends it plans. The server writes it in one
   transaction together with the broadcast jobs, which belong to the server.
   A write trait would have one implementer and one caller, both in the
   server.
4. **Actions become an enum.** `Action::Send(SendInput)`, serde-tagged so the
   stored JSON (`{"name": "send", "input": {…}}`) does not change. This
   deletes the `Action` trait, the `Actions` registry, `Invocation`, the
   `"unknown action"` path and the API's hand-made name check.
5. **The package plans sends and the server turns them into jobs.**
   `Payout { from, asset, transfers }`, one per sender, in order. The server
   maps each one to its `broadcast::SendIntent` → `Broadcast::job`.
6. **Selector meaning moves to the package, selector SQL stays in the
   server.** `Selector` and its validation are package types.
   `Selector::sql` becomes a free function in `models/wallet_index.rs`'s
   `db` (the model owns its SQL); `wallet_index_selector.rs` goes away.
7. **The round keeps its single snapshot.** The server opens one read
   transaction and drives the package's async `decide` inside it with one
   `Handle::block_on`. Today it calls `block_on` once per fee estimate
   instead.
8. **Step one changes no behaviour.** The bugs found below (section 6) are
   fixed in their own later steps, each with its own test.
9. **No change to `sdk/base`, `sdk/wallets` or `packages/crypto` is needed.**

## 1. Inventory: what moves, what stays

| Today | Item | Goes to |
|---|---|---|
| `models/trigger.rs` | `Invalid`, `This`, `Target` (+`asset`, `validate`), `Gate`, `Filter`, `Compare` (+`validate`, `reads_this`), `Balance`, `Aggregate`, `Operator` (+`holds`), `Amount` (+`validate`, `reads_this`), `text` (Decimal as text), `Logic<L>` (+`validate`, `eval`, `Form`, `Combinator`) | **moves** `sdk/trigger/src/definition.rs` |
| `models/trigger.rs`, `usecase/trigger.rs` (uncommitted in the working tree as this was written) | `Amount::Times(amount, factor)` with its validation, and `up` (round up to a whole atomic unit) | **moves** with `Amount` and evaluation |
| `models/trigger.rs` | `Action { name, input: Value }` | **moves, reshaped**: `enum Action { Send(SendInput) }` |
| `models/trigger.rs` | `Stats`, `LastRound` | **moves** `sdk/trigger/src/round.rs` (`ToSchema` behind `openapi`) |
| `models/trigger.rs` | `Trigger::validate` | **splits**: the definition's rules (selector, filter reads `This`, check can hold, send shape) **move** to `trigger::Trigger::validate`. The id slug rule **stays** on the row |
| `models/trigger.rs` | `Trigger` row (diesel, `Json<…>` columns, `group_id`, `enabled`, times, `last_round`), `Keyed`, `db::{insert, count, get_by_id, find_page, update, find_enabled, set_last_round, delete}` | **stays**. Columns are typed by package types. Adds `Trigger::new(id, network, group, definition, enabled, at)` and `From<Trigger> for trigger::Trigger` |
| `models/trigger.rs` tests | `wallets_are_named_on_purpose`, `a_filter_reads_the_wallet_it_tests`, `a_check_can_hold` | **move** to package (without the id part) |
| `models/wallet_index_selector.rs` | `Selector` enum, `validate`, `asset` | **moves** `sdk/trigger/src/selector.rs` |
| `models/wallet_index_selector.rs` | `Selector::sql` | **stays**, as `wallet_index::db::picked(&Selector, &mut binds)`. The file is deleted |
| `models/wallet.rs` | `is_tag` | **splits**: the tag rule moves to `trigger::is_tag` (selectors validate tags). `wallet::is_tag` calls it, and the trigger id check uses it too |
| `models/wallet_index.rs` | `WalletIndex` (API row), `IndexState`, all `db` statements except below | **stays** |
| `models/wallet_index.rs` | `Selection` (`new`, `only`, `insert`, `ids`, `spends`, `address`, `Deref`), `WalletIndex::held` | **move** as `trigger::Selection` over a lean `trigger::Index` (balances decoded once) |
| `models/wallet_index.rs` | `WalletIndex::spendable` | **stays only as the conversion** `TryFrom<WalletIndex> for trigger::Index` (decode, refuse a negative confirmed balance). No other caller |
| `models/wallet_index.rs` | `db::select`, `db::read` | **stay**. They return `Vec<trigger::Index>` |
| `models/wallet_index.rs` | `db::find_by_target` | **splits**: `This{}` / `This{asset}` / `Select` resolution and the sibling count check **move** to the package. The server keeps only `db::find(network, wanted: &[(Address, Asset)], confirmed)` (fully indexed ones, one query). `find_by_ids` (test only) goes |
| `models/wallet_index.rs` | `db::find_held` (`base::Holdings`) | **stays**. It is fee pricing, used by the estimator, not read by the engine |
| `models/trigger_run.rs` | `TriggerRun` row, `Run`, `RunJob`, `db::{insert, find_page, count_by_hour}`, view `trigger_run_with_jobs` | **stays** (runs reference job ids: queue knowledge). `stats: Json<trigger::Stats>` |
| `usecase/trigger.rs` | `Trigger` (decide, check, filter, gate, compare, amount, target cache), `Triggers` (confirmed + enabled, decide in id order), `Decision`, `Context` | **moves** to `sdk/trigger/src/round.rs`. The file is replaced by the SQLite `Snapshot` (~60 lines) |
| `usecase/workers/trigger/actions.rs` | `Action` trait, `Actions`, `Invocation`, `Transfer` (the send plan), `SendInput` (+`validate`) | **moves**: `SendInput` to `definition.rs`, the send plan to `round.rs` as `Payout`s. Trait, registry and invocation are **deleted** |
| `usecase/workers/trigger/mod.rs` | `Round` worker: `KIND`, `POLICY` (Skip, unique, 2), `topic` (`chain:network`), read → decide → refusal log → one write → notify → Idle/Done | **stays**, shrinks to wiring: build `Context`, read+decide, log refusals, write. Becomes `workers/trigger.rs` |
| `api/triggers.rs` | routes, `Definition`, `BatchRequest`, `CreateResponse`, `TriggerRun`/`RunOutcome`/`RunJob`, `RunsResponse`, `now` | **stays**. `Definition::trigger` builds a `trigger::Trigger`, calls its `validate`, checks network and send asset (404s), builds the row. `RunOutcome` stays: it is a fold over job states |
| `usecase/stats.rs`, `workers/prune.rs`, `workers/index.rs` (`Round::enqueue`), `main.rs` | counts, prune of rounds, queueing at tip, wiring | **stay**, untouched |
| `init.sql` | `trigger`, `trigger_run`, `trigger_run_with_jobs` | **stay**, unchanged |

The server goes from ~2,000 lines of trigger code to about 700 (row, run
row, snapshot, worker, API). The package is about 900 lines plus tests.

## 2. The package's public API

```text
sdk/trigger/src/
  lib.rs          re-exports
  definition.rs   Trigger, Logic, Filter, Gate, Compare, Aggregate, Operator,
                  Balance, Amount, Target, This, Action, SendInput, Invalid, is_tag
  selector.rs     Selector (validate, asset)
  selection.rs    Index, Selection
  snapshot.rs     Snapshot, Enabled
  round.rs        Context, decide, Round, Decided, Stats, LastRound, Payout
  memory.rs       Memory (feature `testing`)
```

### Definitions

These are the types of `models/trigger.rs`, byte for byte in JSON. Two
changes:

```rust
/// A trigger's definition: what it picks, keeps, waits for and does. Its id,
/// network, group and enabled flag are its store's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trigger {
    pub selector: Selector,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Logic<Filter>>,
    pub check: Logic<Gate>,
    pub action: Action,
}

impl Trigger {
    /// Wallets named on purpose, a filter whose every compare reads `This`,
    /// a filter and check that can hold, a send that is not many to many.
    pub fn validate(&self) -> Result<(), Invalid>;
}

/// What a trigger does when its check holds. JSON: `{"name": "send", "input": {…}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name", content = "input", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Send(SendInput),
}
```

`Selector`, `Target` and `This` also derive `PartialOrd, Ord`, so a resolved
target is cached by the value itself, not by its JSON text
(`serde_json::to_string(target)` today).

### Selection

```rust
/// One asset of one address, as a round reads it at the confirmed block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Index {
    pub address: Address,
    pub asset: Asset,
    /// Its wallet's key is kept: it may send.
    pub keyed: bool,
    /// Spendable at the confirmed block: what a reorg cannot undo, less what
    /// is on its way out (`Balance::Confirmed`).
    pub confirmed: Decimal,
    /// What it holds once every pending transaction lands (`Balance::Pending`).
    pub pending: Decimal,
}

impl Index {
    /// `chain.network.address.asset`: what `Selector::Ids` names.
    pub fn id(&self) -> String;
    pub fn held(&self, balance: Balance) -> &Decimal;
}

/// Indexes of one network, by (address, asset): `This`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection(BTreeMap<(Address, Asset), Index>);
// only(&Index), spends(to: Option<&Address>) -> Spends, address() -> Option<&Address>,
// Deref to the map, FromIterator<Index>
```

The key is (address, asset), the identity `base` already uses. The server's
id string is only needed by `Selector::Ids`, and `Index::id` derives it.
`Selection` no longer carries `network` or `confirmed`: the round holds them.

### Evaluation

```rust
/// What every trigger of a round decides with.
#[derive(Clone)]
pub struct Context {
    pub network: Network,
    /// The network's assets: a `value` in display units → atomic.
    pub assets: Assets,
    pub fees: Arc<dyn FeeEstimator>,
    /// Unix ms; `since` gates compare against it.
    pub now: u64,
}

/// One round of `snapshot`'s network: `None` while its confirmed position is
/// unknown. Every enabled trigger decides at that position, in id order; one
/// that cannot decide is refused (its `LastRound` says why) and the others go
/// on.
///
/// # Errors
///
/// When the confirmed position or the triggers cannot be read: the round
/// is retried.
pub async fn decide(snapshot: &mut dyn Snapshot, context: &Context) -> anyhow::Result<Option<Round>>;

/// What one round decided.
pub struct Round {
    /// When it decided, Unix ms (`context.now`).
    pub at: u64,
    /// The confirmed position it decided at.
    pub confirmed: u64,
    /// Every enabled trigger, in id order.
    pub decided: Vec<Decided>,
}

/// One trigger's part of a round.
pub struct Decided {
    pub trigger: String,
    /// What it saw, and why it was refused: written every round.
    pub last: LastRound,
    /// What it sends, one per sender; empty: it does not act (no run).
    pub payouts: Vec<Payout>,
}

/// One sender's transfers of one asset, in order: one broadcast.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payout {
    pub from: Address,
    pub asset: Asset,
    /// (recipient, atomic amount), each above zero.
    pub transfers: Vec<(Address, Decimal)>,
}
```

`Stats { selected, kept, checked, jobs }` and `LastRound { at, confirmed,
stats (flattened), refused }` keep their JSON. `jobs` is `payouts.len()`.
`at` and `confirmed` become `u64` (as `tasks::now()` and `base` positions
are); the JSON is the same.

Run outcomes (`RunOutcome`: pending / succeeded / failed / mixed) are **not**
in the package. They fold the states of queue jobs, which the package never
sees. They stay in `api/triggers.rs`.

## 3. The store interface: `Snapshot`

What evaluation reads today, from `usecase/trigger.rs`, `actions.rs` and
`wallet_index.rs`:

| Read | When | Today |
|---|---|---|
| confirmed position of the network | once per round | `network_index::db::find_confirmed` |
| enabled triggers by id + latest run's `started_at` | once per round | `trigger::db::find_enabled` (joins `last_run`) |
| indexes a selector picks, fully indexed at confirmed, with spendable and pending balances, `keyed` | per trigger (selector), per `Select` target | `wallet_index::db::select` |
| the `asset` sibling of each wallet in `This`, fully indexed | per `This{asset}` target | `find_by_target` → `read(Ids)` |
| fee of `Spends` with an optional recipient | per `Fee` amount | `FeeEstimator::estimate` (reads `Holdings` itself) |
| asset decimals | per `value` | `Assets` |

The fee and the assets are capabilities the server already has (`base`), so
they go in `Context`, not in the port. The rest is the port:

```rust
/// Everything one round reads from its store, all from one consistent view:
/// a round opens one, reads through it, and drops it. A SQL store holds one
/// read transaction; balances and the confirmed position never move under a
/// round.
///
/// An index is *fully indexed* when it is indexed through a live block at or
/// above the confirmed position; only fully indexed ones are read. That set is
/// the universe `Selector::Not` is relative to.
pub trait Snapshot: Send {
    /// The network's confirmed position; `None` while unknown.
    ///
    /// # Errors
    ///
    /// When the store cannot read.
    fn confirmed(&mut self) -> Future<'_, anyhow::Result<Option<u64>>>;

    /// The network's enabled triggers, by id.
    ///
    /// # Errors
    ///
    /// When the store cannot read or a stored trigger does not decode.
    fn triggers(&mut self) -> Future<'_, anyhow::Result<Vec<Enabled>>>;

    /// The fully indexed indexes at `confirmed` that `selector` picks
    /// (`Ids`: by `Index::id`; `Asset`; `Tag`: its wallet carries it;
    /// `Untagged`; `All`/`Any`/`Not` as AND/OR/NOT within the universe).
    ///
    /// # Errors
    ///
    /// When the store cannot read, or a confirmed balance is negative.
    fn select<'a>(
        &'a mut self,
        selector: &'a Selector,
        confirmed: u64,
    ) -> Future<'a, anyhow::Result<Vec<Index>>>;

    /// Of `wanted`, the fully indexed indexes at `confirmed`; the others are
    /// left out.
    ///
    /// # Errors
    ///
    /// As [`Snapshot::select`].
    fn find<'a>(
        &'a mut self,
        wanted: &'a [(Address, Asset)],
        confirmed: u64,
    ) -> Future<'a, anyhow::Result<Vec<Index>>>;
}

/// An enabled trigger as a round reads it.
pub struct Enabled {
    pub id: String,
    pub trigger: Trigger,
    /// When its latest run started, Unix ms; `None`: never ran.
    pub last_run: Option<u64>,
}
```

Why it looks like this:

- **One trait, reads only.** A round's writes are one transaction that
  mixes trigger rows with job rows. Jobs are the server's (`task` model,
  `Broadcast::job`, `Notifying::notify`). The package returns `Round`, and
  the server writes it (section 4). Tests check the returned value, with no
  fake writer.
- **`&mut self`, one per round.** This is what makes "one snapshot" a type
  rule: the server builds it over the `&mut SqliteConnection` of one read
  transaction. A store over an async database would hold a repeatable-read
  transaction the same way. `tasks::Store` uses `&self` because it is
  shared; a snapshot is not.
- **Async with `base::Future` and `anyhow`**, as `base::BlockStore`,
  `WalletIndexStore` and `Holdings` are. A third party on an async database
  implements it directly. SQLite answers with ready futures.
- **`find` by (address, asset), not by `Target`.** `This{asset}` sibling
  building (one per wallet, de-duplicated) and the rule "fewer found than
  wanted: unknown" are evaluation rules. They move into the package, and
  the store only answers which of these pairs are fully indexed.
- **`confirmed` is passed back explicitly** to `select` and `find`, so the
  server's implementation holds no state beyond the connection.
  `Selection::confirmed_at` goes away.

## 4. The server after

### `usecase/trigger.rs`: the SQLite snapshot

```rust
/// `trigger::Snapshot` over one read transaction: every statement is a model's.
pub(crate) struct Sqlite<'c> {
    connection: &'c mut SqliteConnection,
    network: Network,
}

impl trigger::Snapshot for Sqlite<'_> {
    fn confirmed(&mut self) -> Future<'_, anyhow::Result<Option<u64>>> {
        let found = network_index::db::find_confirmed(self.connection, &self.network);
        Box::pin(std::future::ready(found.map_err(Into::into).and_then(|at| at.map(uint).transpose())))
    }
    fn triggers(&mut self) -> Future<'_, anyhow::Result<Vec<Enabled>>> { /* find_enabled → Enabled */ }
    fn select<'a>(&'a mut self, selector: &'a Selector, confirmed: u64) -> … { /* wallet_index::db::select */ }
    fn find<'a>(&'a mut self, wanted: &'a [(Address, Asset)], confirmed: u64) -> … { /* wallet_index::db::find */ }
}
```

Each method is one model call wrapped in `ready`. The SQL stays where it is:
`wallet_index::db::{select, find, picked}`, `trigger::db::find_enabled`,
`network_index::db::find_confirmed`. `find_enabled` may narrow to
`trigger_run.started_at`, since that is all it needs.

### `workers/trigger.rs`: the `Round` worker

Policy, topic, `KIND`, `enqueue` from the index job: unchanged. `run`:

```rust
async fn run(&self, cx: tasks::Context<'_, Target, ()>) -> Ack {
    let network = cx.input().network();
    let round = async {
        let service = self.payments.service(&network)?;
        let context = trigger::Context {
            network: network.clone(),
            assets: service.assets.clone(),
            fees: service.wallets.factory.fees(),
            now: tasks::now(),
        };
        // One read transaction; the estimator's own reads run beside it.
        let runtime = tokio::runtime::Handle::current();
        let decided = self.db.read(move |connection| {
            let mut snapshot = usecase::trigger::Sqlite::new(connection, &context.network);
            runtime.block_on(trigger::decide(&mut snapshot, &context))
        });
        let Some(round) = decided.await? else { return anyhow::Ok(Ack::Idle) };
        for refused in round.decided.iter().filter(|d| d.last.refused.is_some()) {
            /* trigger.refused warn, as today */
        }
        if round.decided.is_empty() { return anyhow::Ok(Ack::Idle) }
        let acted = round.decided.iter().any(|d| !d.payouts.is_empty());
        self.db.write(move |connection| save(connection, &round)).await?;
        if !acted { return anyhow::Ok(Ack::Idle) }
        self.queue.notify();
        anyhow::Ok(Ack::Done)
    };
    round.await.unwrap_or_else(|error| Ack::Retry(format!("{error:#}")))
}

/// Every trigger's last round, and every acting one's jobs and run: one write.
fn save(connection: &mut SqliteConnection, round: &trigger::Round) -> anyhow::Result<()> {
    let at = int(round.at)?;
    for decided in &round.decided {
        trigger::db::set_last_round(connection, &decided.trigger, &decided.last)?;
        if decided.payouts.is_empty() { continue }
        let mut jobs = Vec::new();
        for payout in &decided.payouts {
            let job = Broadcast::job(&SendIntent::from(payout))?;
            jobs.push(task::db::insert(connection, &Row::try_from(&job)?, at)?.to_string());
        }
        trigger_run::db::insert(connection, &TriggerRun::new(&decided.trigger, at, jobs, decided.last.stats))?;
    }
    Ok(())
}
```

`SendIntent::from(&trigger::Payout)` lives in `broadcast.rs`, next to
`SendIntent`. It is the only place that knows the job input format.

### `api/triggers.rs`

`Definition` keeps all its fields. It cannot `#[serde(flatten)]` a
`trigger::Trigger`, because serde does not support `flatten` together with
`deny_unknown_fields`. Its `selector`, `filter`, `check` and `action` are
package types, and `Definition::trigger`:

1. refuses a bad id (`trigger::is_tag`), with 400 as today;
2. `payments.service(&network)?`, with 404 as today;
3. `let Action::Send(send) = &definition.action;` then
   `asset_info(send.asset)`, with 404 as today;
4. builds `trigger::Trigger { selector, filter, check, action }` and calls
   its `validate()`, with 400 as today;
5. `models::Trigger::new(…)`.

The `"unknown action"` 400 now comes from serde: an unknown `name` is a
malformed body. Same status, different text.

## 5. Actions: the boundary

- **Package**: what to send. Sources and destinations resolve at the
  confirmed block. It refuses many↔many, a source in another asset, a
  source not keyed here and a target not fully indexed. Amounts are
  evaluated per source or per destination (`This` is the walked side), and
  amounts ≤ 0 or unknown are skipped. Transfers are grouped per sender, in
  source-then-destination order. Out comes `Vec<Payout>`.
- **Server**: how it is sent. `Payout` → `SendIntent` → `Broadcast::job`,
  job rows, `trigger_run` row with the job ids, `notify()`. The broadcast's
  own guard (check spendable and record pending in one write) is unchanged.
- No action trait: `match` on `Action`. A new action means a new variant
  with its own `Decided` output field. That is fine while there is one
  action; a registry comes back only if third parties need their own.

## 6. Correctness requirements to keep

| # | Requirement | Where it is enforced | Tested today |
|---|---|---|---|
| R1 | Decide at the confirmed position, never the tip; no round while unknown | `Triggers::new` (`find_confirmed`) | universe: `pipe.rs` `leaves_pick_exactly_their_fully_indexed_indexes`, `combinators_are_set_operations_within_the_universe`; **no test pins "confirmed, not tip"** |
| R2 | Only fully indexed indexes (live block ≥ confirmed); `Not` relative to them | `wallet_index::db::read` | `pipe.rs` `leaves_pick…`, `a_tag_or_indexing_brings_indexes_in` |
| R3 | `Balance::Confirmed` = spendable (confirmed less outgoing not yet in it); `Pending` = once every pending tx lands | view `wallet_index_with_balance`, `WalletIndex::held` | `pipe.rs` `balances_are_confirmed_exact_and_folded`, `a_pending_send_is_taken_off…`, `a_pending_balance_counts…`; `storage.rs` `totals_hold` |
| R4 | One snapshot per round: every read sees the same confirmed position and balances | one `db.read` | implicit, no test |
| R5 | Filter per index (`This` = it alone), check over the kept set; written order, short-circuit; errors stay errors (under `Not` too) | `Trigger::filter`, `Logic::eval` | `pipe.rs` `a_filter_keeps_each_index_on_its_own`, `logic_stops_early_and_errors_stay_errors` |
| R6 | Unknown is `false`: a `This{asset}` sibling missing, no asset, `min`/`max` of none | `compare`, `find_by_target` | `pipe.rs` `counterparts_must_be_fully_indexed`, `aggregates_and_operators` |
| R7 | `since` reads the latest run's start; no run passes | `Trigger::gate` | `pipe.rs` `since_reads_the_last_run`, `triggers_and_their_runs_are_stored` |
| R8 | Fees via `FeeEstimator` over `Spends`, one recipient address priced as it is, none or several as new; holding nothing costs 0 and asks nothing | `Amount::Fee`, `Selection::spends` | `pipe.rs` `a_fee_is_priced_for_its_one_recipient_or_a_new_address`, `a_wallet_holding_nothing_costs_no_fee_and_asks_nothing`, `holders_are_keyed_wallets…` |
| R9 | Counts compare with whole numbers (create and decide) | `Compare::validate`, `compare` | `routes.rs` `a_count_compares_with_a_whole_number…`, `aggregates_and_operators` |
| R10 | `value` in display units → atomic by the asset's decimals; too many decimals is an error | `Amount::Value`, `AssetInfo::atomic` | `pipe.rs` `values_must_fit_their_asset`, `amounts_evaluate_in_atomic_units` |
| R11 | Send shapes, keyed sources, per-sender grouping, skip ≤ 0 | `Transfer::run` | `pipe.rs` `a_send_pays_per_source_or_per_destination` |
| R12 | A refusal is per trigger: the others decide; stats keep what was counted before the refusal | `Triggers::decide`, `Round::run` | **no test** of the refusal path or its `last_round.refused` |
| R13 | Last round written every round for every enabled trigger, acted or not | `Round::run` | `cases/backlog.rs` (`last_round.confirmed`, `refused` null) |
| R14 | A run is written only when the action started ≥1 job, with its job ids and stats | `Round::run` | `routes.rs` `a_trigger_run_shows_its_jobs_and_how_they_ended` (seeded) |
| R15 | One write per round: all or none | `Round::run` | `cases/partial.rs` |
| R16 | A round is unique per network and skips while a later job of its network is unfinished | `Round::POLICY`, topic | `cases/skipped.rs`, `cases/backlog.rs` |
| R17 | Stored JSON forms unchanged (API and UI read them) | serde attributes | `pipe.rs` `the_sweep_trigger_round_trips_through_json` |
| R18 | Validation: wallets named on purpose, filter reads `This`, check can hold, no `This`→`This` send | `Trigger::validate`, `SendInput::validate` | `models/trigger.rs` tests |

### Ambiguities and bugs found (not fixed by the move)

- **B1 A compare's asset can be the wrong one.** `compare` takes the asset
  from `of.asset()` or else from the first index of `This`. A check like
  `{of: {select: {ids: [treasury.native]}}, value: {value: "0.9"}}` on a
  `This` of 6-decimal token indexes converts `0.9` with the token's
  decimals and compares that with native balances. The same goes for
  `Amount::Balance` inside an amount of another asset. Fix: a compare's
  asset is the single asset of the indexes `of` resolved to; mixed assets
  are a refusal; an empty set falls back to the named asset, then `This`'s.
- **B2 Mixed-asset sums.** A selector like `{tag: deposit}` picks native and
  token indexes, and `sum` over them adds native and token atomic units.
  B1's fix (one asset per compare) also refuses this.
- **B3 Fees are not cached.** Each `Fee` term calls the estimator again.
  eval.md chose one fee per evaluation so that check and action agree (075
  §6.7); it was never built. Two calls can straddle a cache refresh, so the
  check passes on one fee and the amount uses another.
- **B4 The `Select` target cache is keyed by `This`'s ids.** In a filter
  (one `This` per index) a `{select: …}` term runs one SQL query per
  selected index, and the same treasury is read again by every trigger. It
  should be cached per round by the selector alone; the snapshot makes that
  safe.
- **B5 Empty source or destination is a silent no-op** (075 F8). A send
  whose other side resolves empty returns no payout with no refusal.
- **B6 Value precision is checked only at decide time** (075 F8). It is
  refused every round, never at create, even when the asset is known (the
  send's `asset`, a compare whose `of` names one).
- **B7 `count` with `"1.0"` is refused** (scale 1). Strict but clear; keep.
- **B8 `find_page` hides undecodable stats** (`unwrap_or_default`), so a
  broken row shows zeros.
- **B9 The estimator reads outside the snapshot.** Holdings and UTXOs come
  on a second pool connection while the round holds one (075 §1). Prices
  only, and conservative; unchanged.
- **B10 `routes.rs` seeds `{"name":"send","input":{}}`.** With a typed
  `Action` that row no longer decodes. The runs page never reads it, but
  the fixture should carry a full send input.

## 7. Testing plan

**`sdk/trigger` tests** (over `Memory`, a fixed-fee estimator that counts
calls, `futures_executor::block_on`):

- `Memory` (feature `testing`, like `tasks::MemoryStore` is the reference
  store): `confirmed`, `triggers`, and kept indexes with tags and a
  `fully_indexed` flag. Its `select` evaluates `Selector` in Rust (~30
  lines), so it is also the reference for what a selector means.
- Moved from `pipe.rs`, rewritten over `Memory`:
  `aggregates_and_operators`, `values_must_fit_their_asset`,
  `amounts_evaluate_in_atomic_units`,
  `a_fee_is_priced_for_its_one_recipient_or_a_new_address`,
  `a_wallet_holding_nothing_costs_no_fee_and_asks_nothing`,
  `counterparts_must_be_fully_indexed` (the "fewer found: unknown" half),
  `logic_stops_early_and_errors_stay_errors`, `since_reads_the_last_run`,
  `the_sweep_trigger_round_trips_through_json`,
  `a_filter_keeps_each_index_on_its_own`,
  `a_send_pays_per_source_or_per_destination`, the compare half of
  `a_pending_balance_counts_what_is_already_on_its_way`.
- Moved from `models/trigger.rs`: the three validation tests.
- New, for the gaps above:
  - no round while confirmed is unknown, and `select` is asked at the
    confirmed position (R1);
  - one trigger refused, the others decide, and its `LastRound` keeps
    `selected`/`kept` counted before the error (R12);
  - every enabled trigger gets a `LastRound` (R13);
  - `jobs == payouts.len()`, and a trigger whose check held but that sends
    nothing has no payouts (R14);
  - payouts grouped by sender in order (R11).

**Server tests** (SQL, HTTP, workers):

- `pipe.rs` keeps the store facts: `tags_are_kept_once_listed_and_removed`,
  `leaves_pick…`, `combinators…`, `a_tag_or_indexing_brings_indexes_in`,
  `balances_are_confirmed_exact_and_folded`, `a_pending_send_is_taken_off…`,
  `holders_are_keyed_wallets…`, `triggers_and_their_runs_are_stored`,
  `a_tag_picks_its_wallets_indexes_on_their_network_only`, and the SQL half
  of `a_pending_balance_counts…`. They call `usecase::trigger::Sqlite`
  through the `Snapshot` trait, so they also test the implementation: `find`
  leaves out a sibling that is not fully indexed, and `select` refuses a
  negative confirmed balance.
- `routes.rs`, `cases/*` (`backlog`, `partial`, `skipped` and the other
  trigger cases): unchanged, still green. They cover R13–R16 end to end.
- Optional later: a `trigger::testing::snapshot_contract` that both `Memory`
  and the SQLite snapshot run, as `tasks::testing::store_contract` does
  (open question 5).

## 8. Migration steps

Each step builds the workspace and leaves `cargo test` green. Steps 1–4
change no behaviour and no JSON.

1. **Package with definitions.** Add `sdk/trigger` to the workspace and
   move `definition.rs` (all definition types, `Logic`, `Invalid`,
   `is_tag`), `selector.rs` (`Selector`, `validate`, `asset`), `Stats` and
   `LastRound` (feature `openapi` for `ToSchema`). In the server:
   `models/trigger.rs` keeps the row and `db`; `Selector::sql` becomes
   `wallet_index::db::picked`; `wallet_index_selector.rs` is deleted;
   `wallet::is_tag` delegates. The three model validation tests move.
2. **Action enum.** `Action::Send(SendInput)`, with `SendInput::validate`
   folded into `Trigger::validate`. In the server, `actions.rs` matches the
   enum instead of looking up a name; the API drops its name check and
   `SendInput::deserialize`; the `routes.rs` fixture gets a full input
   (B10).
3. **Evaluation in the package, added beside the server's.** Add
   `selection.rs`, `snapshot.rs`, `round.rs` (`decide`, `Round`, `Decided`,
   `Payout`), and `memory.rs`, with the package tests of section 7. The
   server is untouched; both engines exist for one step.
4. **The server switches.** `usecase/trigger.rs` becomes `Sqlite` (the
   `Snapshot`); the `Round` worker calls `trigger::decide` and `save`;
   `SendIntent::from(&Payout)` goes in `broadcast.rs`; `actions.rs`,
   `Selection`, `WalletIndex::held`, `find_by_target` and `find_by_ids` are
   deleted; `wallet_index::db::{select, find}` return `trigger::Index`.
   `pipe.rs` drops the tests that moved and calls the snapshot for the
   rest.

Then, one step each, each with its test, only once agreed (open questions):

5. Per-round caches: selections by selector (B4), fees by spends (B3).
6. One asset per compare (B1, B2).
7. Refuse an empty source or destination (B5).
8. Validate value precision at create where the asset is known (B6),
   as a package `Trigger::validate_assets(&Assets, &Network)`.

## 9. Open questions (recommendation first)

1. **Crate name.** The server already has `models::trigger`,
   `usecase::trigger` and `workers::trigger`, so a crate named `trigger`
   must be written `::trigger::…` inside those modules. *Recommend:* keep
   the crate `trigger` (as asked) and use `::trigger` there. Renaming
   server modules or aliasing the dependency hides the real name.
2. **Async port driven by one `block_on`, or a sync port?** A sync
   `Snapshot` fits diesel exactly, but it shuts out async stores and
   differs from `base`'s style. *Recommend:* async, as above. The server
   pays one `block_on` per round (today it pays one per fee).
3. **`utoipa` in the package.** *Recommend:* an optional `openapi` feature
   deriving `ToSchema` on `Stats` and `LastRound`, rather than mirror DTOs
   in the server.
4. **Action enum instead of a registry.** *Recommend:* yes; there is one
   action. Bring back a trait only if third parties need their own actions.
5. **A snapshot contract suite now?** *Recommend:* not now. `Memory` is the
   reference, and `pipe.rs` pins the SQL. Add the suite if the two ever
   disagree.
6. **Fix B1/B2 (one asset per compare, mixed sets refused)?** *Recommend:*
   yes, as step 6. Today such a trigger silently compares in the wrong
   units.
7. **Per-round caches (B3, B4)?** *Recommend:* yes, as step 5. The fee
   cache makes check and action agree, as eval.md decided.
8. **Empty source or destination (B5) and create-time precision (B6)?**
   *Recommend:* refuse both (075 Q5, §6.6).
9. **Should a failed run count for `since`?** (075 Q3, still open.)
   *Recommend:* out of scope here. The package reads `last_run` as the
   store gives it, so the rule stays a store query either way.
10. **Should the `Round` worker move into the package, generic over
    `tasks`?** *Recommend:* no. Its topic, policy and job rows tie it to
    the server's broadcast worker; the package stays free of the queue.
