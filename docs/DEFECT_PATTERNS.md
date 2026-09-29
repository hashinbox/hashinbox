# Defect patterns

How to recognise that something in this codebase is wrong, before a human has
to notice it by reading.

Every pattern below is stated as a **question with a checkable answer**, not as
taste. Every example is real, from this repository, with the line it was found
at. A reviewer — human, Codex, or an agent — should be able to apply these
without knowing the domain.

**The examples are deliberately historical, and stay that way once fixed.** Every
one of them was a real defect in this repository, which is what makes the test
above it credible — a pattern illustrated by a defect somebody actually shipped
is worth more than one illustrated by a hypothetical. Do not delete an example
because the code was since repaired, and do not annotate each one with its
current status; this document teaches recognition, it does not track work. Open
work lives in `docs/design/`.

Three kinds of defect, in the order they cost the most:

1. **Wrong responsibility** — the thing works, but it is owned by the wrong
   module. Costs the most because fixing it later means moving code across
   crate boundaries.
2. **Wrong name** — the name does not predict the contents. Costs next, because
   every reader pays it on every read, forever.
3. **Nonsense** — code that exists only to serve a problem that should not
   exist. Costs least to delete and is the easiest to miss, because it always
   looks locally reasonable.

---

## 1. Wrong responsibility

### 1.1 The courier

**Test:** does this type hold a value it never reads, only passes on?

If A obtains X, stores X, and hands X to B, and only B uses X — then X belongs
to B, or to whatever A got it from. A is a courier and the field should not
exist.

*Found here:* `HistoryAssets` carried an ERC-20's `decimals` from the chain
config, through `WalletFactory::assets()`, through `Wallets`, to `apps/api` —
the only caller that scaled anything. The chain already knew the number and had
already verified it against the contract's own `decimals()`
(`sdk/chains/ethereum/src/config.rs:150`). The wallet used it for nothing.

### 1.2 Two hats

**Test:** does one type implement both a generic mechanism and a domain
abstraction built on that same mechanism?

The symptom is a name collision inside the type: the same verb means two things
at two levels.

*Found here:* `Repository` was both the key-value backend and the domain store,
so `BlockStore::get` (fetch this network's checkpoint) collided with
`Repository::get::<T>` (fetch bytes at this key). The collision forced every
trait method to delegate to an inherent method with a *different* name —
`get` → `read_checkpoint`, `add` → `write_block`, `purge_network` → `purge` —
and that rename was the only reason those methods existed. Splitting the hats
deleted seven wrapper methods and gave every operation one name.

**Corollary — the rename tell:** when a trait method delegates to an inherent
method whose only difference is the name, ask *why the name had to change*. The
answer is almost always a second responsibility in the same type.

### 1.3 Knowledge that leaked upward

**Test:** does a composition root, a binary, or an edge know the internals of a
thing it merely assembles?

*Found here:* `apps/server/src/chains/{bitcoin,ethereum,solana}.rs` knew each
chain's RPC stack, interpreter and wallet provider from outside, so adding a
chain meant editing the server. Now each chain declares its own `Config` →
`Node` → `ComposedChain` and the server only dispatches.

### 1.4 Reading from the wrong source

**Test:** is a value that the system already knows being re-read from an
external service?

*Found here:* balance was read from the node (`eth_getBalance`) while the index
held every movement needed to compute it. The node answer and the indexed
answer could disagree, and nothing detected it. Balance now folds from the
index, and the node reading survives only as an explicit *witness* that reports
disagreement without changing the number.

---

## 2. Wrong name

### 2.1 The name does not predict the contents

**Test:** read the name, write down what you expect to find, open it, compare.

*Found here:*
- `ChainCollection` holds **one chain's networks**, not a collection of chains.
  Every chain declares `pub const CHAINS: ChainCollection`, which reads as
  bitcoin + ethereum + solana (`sdk/chains/*/src/native/network.rs`).
- `KeyPair` holds **one** key; the public key is derived on demand
  (`packages/crypto/src/key_pair.rs:11,32`).
- `Failover.endpoints` holds **clients**, while `json_rpc::Endpoint` is a
  separate type meaning a URL (`packages/json-rpc/src/failover.rs:16,52`).

### 2.2 The name is a metaphor, not the contents

**Test:** does the name say what kind of thing it is, or only that something
important happens inside?

*Found here:* `engine/` — 11 files, 2,800 lines, all of them serving one sync
pass. The module doc had to explain *"How a pass actually runs"* because the
name could not. Its sibling `storage/` is named for its contents and needs no
such sentence.

### 2.3 One name, two concepts

**Test:** grep the name. Does it mean the same thing at every hit?

*Found here:*
- `Payment` (`sdk/base/src/transaction.rs:231`) and `Transfer`
  (`sdk/wallets/src/sender.rs:12`, and each chain's `native::Transfer`) are one
  concept under two names — and the field is `transfers: Vec<Payment>`, which
  disagrees with itself in a single line (`:249`).
- `config::Retry` is a configuration value; `json_rpc::Retry` is a client
  decorator. Both are named in one file (`packages/config/src/retry.rs:16,92`).

### 2.4 The name collides with something every reader already knows

**Test:** does this shadow a std type, a universal module name, or a prelude
item?

*Found here:*
- `config::Sync` shadows `std::marker::Sync`, and the crate README recommends
  the exact import line that then breaks the next `Send + Sync` bound
  (`packages/config/README.md:62`).
- the crate named `config` versus the `mod config;` that nearly every binary
  has. Legal — a local module shadows the extern prelude — but it made the
  dependency unreachable by name inside `apps/server`.

### 2.5 The plural that is not a collection

**Test:** is the plural noun a collection of N things, or a fixed set of roles?

A plural names a collection. A fixed pair with distinct roles is not one.

*Found here:* `HistoryAssets` was a two-field pair — the wallet's own asset and
the native asset that pays fees — not a collection. This is also
`CHAIN_LAYOUT.md` §5's rule: a plural noun is a collection and never a trait.

### 2.6 The stutter

**Test:** write the call site. Count how many times one word appears.

*Found here:* `wallets.asset(&tag)?.assets.asset().id` — four `asset`s, three
meanings: look up by tag, the pair, the wallet's own asset, its id.

---

## 3. Nonsense

Code that is locally sensible and globally pointless. Each of these is the
*answer* to a question nobody should have asked.

### 3.1 Generality for exactly one case

**Test:** count the instances the abstraction serves. If it is one, the
abstraction is the defect.

*Found here:* `codec.rs` carried namespace prefixes, cross-namespace rejection
and framing arithmetic for a keyspace that only ever had **one** namespace.
Deleted, replaced by a single constant, same bytes on disk.

### 3.2 A folder that is not a boundary

**Test:** does this directory introduce new types, or only more methods on a
type declared in the parent file?

*Found here:* `sdk/wallets/src/wallets/` held `registration.rs` and `batch.rs`,
both containing nothing but `impl<A> Wallets<A>` blocks. No new type, no trait,
no boundary. The parent file was 122 production lines — nowhere near the
500-line cap that usually justifies a split.

### 3.3 The unreachable branch that a rewiring made unreachable

**Test:** is this arm rejected at runtime for a case the system genuinely has?

*Found here:* `crypto::KeyPair::sign_now` refused `SignatureScheme::Ed25519`
with *"key pair supports secp256k1 only"* — not because Ed25519 was
unsupportable, but because Solana had been wired around `packages/crypto`
entirely and signed with its own key type. The branch was dead evidence of an
inconsistency elsewhere.

**This one has a tell:** it had a `// todo this looks like bs` on it. Comments
expressing confusion are data — the author already detected the defect and
lacked the time to trace it.

### 3.4 Work done to satisfy a checker rather than a caller

**Test:** if this existed only to silence a warning or a lint, what breaks when
it is deleted?

*Found here:* `examples/all_chains/main.rs` built an `assets` vector, never read
it, and called `drop(assets)` at `:205` purely to suppress the unused warning —
which made the dead accumulator look deliberate.

### 3.5 Hand-rolled infrastructure

**Test:** is this code implementing something the layer below already provides?

*Found here:* ~1,900 lines were estimated (and ~840 confirmed) as key encoding,
ordering, framing, corruption detection and row serialization — all of it work a
database does. The tell was that the on-disk format had been re-versioned **by
hand twice**, with two key tags left dead forever and one struct field
undecodable by construction.

### 3.6 Machinery that a structural fix would delete

**Test:** is this coordinating between two things that should not both exist?

This is the expensive one, and `docs/design/030` is an entire document about it:
four structural causes, each of which had grown hundreds of lines of machinery
to manage a problem that disappeared when the cause was removed. The ~950-line
rollback journal died when storage stopped committing the tip.

**Caution — this test has a false positive, and it bit us.** Machinery that
coordinates two concurrent *actors* is not the same as machinery that
coordinates two *owners*. Single ownership deletes the second, not the first.
An agent proposed deleting `NetworkAdmission` on this reasoning and was wrong:
it coordinates a running `watch` against a running pass, which single ownership
does not change.

---

## 4. The test that cannot fail

A fourth kind, found late and worth its own section: the defect is not in the
production code but in the evidence. A test that passes while the behaviour it
names is broken is worse than no test, because it is counted as coverage.

### 4.1 The assertion nothing asserts

**Test:** break the implementation on purpose. Does the suite go red?

If it does not, the test is decorative. This is the only way to find out, and it
costs one minute.

*Found here:* `Completeness::claimed()` was reverted to the form that lets a
birthday of zero claim a complete history, and the **entire `-p indexing` suite
still passed** — there was no test for it anywhere.

### 4.2 The fixture that makes the failure impossible

**Test:** could the defect even be expressed in the shape this fixture builds?

The assertion can be correct, the test still worthless, because the data it runs
against cannot exhibit the failure. This is the subtle one: the test reads as
though it covers the case, and a reviewer checking assertions will agree.

*Found here, twice:*
- `a_movement_named_like_a_fee_still_commits` used `ValueMovement::mint` — a
  **credit**. `direction` is in the primary key, so a credit named `"fee"` can
  never collide with the fee row, which is a debit. The collision the test was
  named for was impossible by construction, and the test passed with the defect
  restored.
- Every fixture block in the repository held **exactly one transaction**, so no
  test in the tree could distinguish any two orderings of a block. The ordering
  defect was not untested; it was unexpressible. `Fixture::block_facts` exists
  now so a contract can build a multi-transaction block.

**The tell for both:** a defect is reported, and the test that should have caught
it already existed and was already green. Do not patch the production code and
move on — ask what the test was actually exercising, because the answer is
usually "not this."

### 4.3 The proof that a pure refactor needs

**Test:** does this change alter observable behaviour? If not, a new test is the
wrong evidence.

Removing a dependency correctly is proved by the type no longer compiling
against the removed capability, and by the deletion of the machinery that only
existed to thread it. Manufacturing a behavioural test for a change with no
behavioural content produces another 4.1.

*Found here:* deleting the maintained balance pair removed the `Novelty` and
`Change` types and their threading through nine functions. That deletion is the
evidence. The test that had asserted the two balance folds agreed was itself the
symptom — a test exists to check that two implementations of one number match
only when there are two implementations of one number.

---

## How to apply this

**Mechanically, every run.** `linter.toml` already enforces several of these:
`rust/struct-noun-naming` (2.1), `rust/redundant-module-prefix` (2.6),
`rust/trait-method-count`, the 500-line production cap, and the banned-name
list. `scripts/check-chains.sh` enforces the chain layout, the native/adapter
boundary, the retired-noun list and the single-file-folder ban in eleven checks.
**If a pattern here can be made mechanical, it should be a rule rather than a
paragraph** — a rule runs on every change, and prose runs when someone
remembers.

**By judgement, on a schedule.** The patterns that cannot be mechanised —
1.1, 1.2, 1.4, 2.2, 3.1, 3.6 — need a reader holding this list. That is what
`/codex:adversarial-review` is pointed at.

**Against the evidence, whenever a defect is reported.** §4 is not a review
pass; it runs at the moment someone says a behaviour is wrong. Mutation is the
cheapest tool here and the most consistently skipped: break it, watch it go red,
restore it byte-for-byte and verify with `diff`.

**Two rules for anyone applying this list.**

*Verify before asserting.* Several confident claims about this codebase have
been false on inspection, including "`crypto::KeyPair` has no callers" (it is
the signing engine for two chains) and "the balance fold can become a SQL
aggregate" (amounts are u256 stored as text; SQLite is i64). State what you
checked.

*Report negative results.* "This looked wrong and is actually correct, here is
why" is worth as much as a finding, because it stops the next reader
re-investigating it.
