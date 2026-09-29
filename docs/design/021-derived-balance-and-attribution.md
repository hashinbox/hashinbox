# 021: Balance is read from a node instead of derived from attributed changes

> "eth_getBalance is an issue, we cannot rely on that — 3rd party nodes might
> each give you different results. We need to track transactions to get tip of
> the chain and be able to get balance at tip − X where X is the confirmation
> count."

> "Point of the indexer is also to mark exactly which transaction made the
> changes to the concrete wallet — I want to see tx id and how it changed the
> wallet."

> "we have to track what changed in each block, that is including the smart
> contract calls."

> "we cannot forget about ERC20 — these need to follow the same concept."

Two of three chains answer `GET /v1/wallets/{id}/balance` by asking a node what
it thinks, and one of those answers at `"pending"`. Neither can name the
transaction that produced the number, and neither can be asked for the number at
a chosen depth. This document defines the attributed change record, enumerates
per chain every way a balance moves that the index does not currently see,
prices the Ethereum trace call that closes the only real gap, specifies
`balance(wallet, confirmations = X)` as a fold with its storage cost, and states
precisely what completeness can and cannot be proven without trusting a node
balance. ERC-20 is carried through every section, not appended to one.

Paths are repository-relative unless the text needs otherwise. Line numbers are
from the working tree on `refactor/chain-unification`. Method inventories come
from `grep -rn` over `sdk apps examples packages` excluding `target/`.

> **Status (reshaped by `025`, not implemented).** The confirmed line each
> chain now states about itself is the natural home for this document's
> `tip − X`: §5.1's `cutoff(X)` is unchanged for `X` at or above the line, and
> below it the fold continues into the in-memory overlay `025` adds. §8.1's
> `observed_height` is already non-null on all three chains, which is what
> §8.1 wanted and could not get: `wallets::Wallets::balance` fills it from the
> index checkpoint for a chain whose node does not say which block it read at.
> `X` now defaults to the line rather than to `minimum_confirmations`, which
> no longer exists. Two constraints this adds to the eventual fold: §5.2's
> negative-fold alarm **must** fold the pending half as a delta on the
> confirmed total, never independently, or a pending spend of a confirmed
> output reads as a false alarm; and the balance wire already reports the two
> halves apart (`amount`, `pending_incoming`, `pending_outgoing`), so the fold
> must produce the confirmed half alone. Nothing in §5.3's "store-side, not
> read-side" changes for that half.

## 1. Verdicts

| # | Piece | Verdict | Effort | Note |
|---|---|---|---|---|
| 1 | Derive balance from indexed movements; never return a node read | **Build** | S | The fold is 120 lines; §2, §5 |
| 2 | `BalanceStore` port + redb range fold + Postgres aggregate | **Build** | M | §5.3 |
| 3 | Bitcoin balance switches from UTXO sum to the fold | **Build** | S | Already complete by construction; §3 |
| 4 | Ethereum consensus-layer withdrawals | **Build** | M | Zero new RPC calls — the bytes are already in hand; §4.1 |
| 5 | Solana: pre/post balances become the authority, not a rejection guard | **Build** | M | Turns a terminal halt into data; §4.2 |
| 6 | Ethereum `debug_traceBlockByHash` for contract-initiated ETH | **Built** | L | The only chain needing an extra call; a node that cannot trace is refused, with no opt-out; §6 |
| 7 | ERC-20 `eth_getLogs` backfill + completeness proof | **Build** | M | The only real completeness oracle in the repo; §7.4 |
| 8 | `balanceOf` / `eth_getBalance` pinned to a block hash **as a witness** | **Build** | S | Detects incompleteness; never the returned amount; §7.3 |
| 9 | Stored running balance per wallet | **Defer** | — | Reorg-unsafe denormalisation; §5.4 |
| 10 | Resulting balance on every history row | **Defer** | — | Makes each page depend on all prior pages; §8.2 |
| 11 | Solana `getSignaturesForAddress` as a completeness proof | **Refuse** | — | Retention-bound and blind to non-signature effects; §7.4 |
| 12 | Archiving raw blocks or traces to storage | **Refuse** | — | `AGENTS.md:352-356` forbids it, and nothing needs it; §6.4 |

## 2. What is true today

### 2.1 Three chains, three unrelated answers

| Chain | Balance source | `observed_at` | Can name the cause? |
|---|---|---|---|
| Bitcoin | Indexed UTXO rows, summed in Rust — `sdk/chains/bitcoin/src/wallet.rs:336-359` | `Some(checkpoint)` (`:355`) | Yes, implicitly |
| Ethereum | `eth_getBalance` at **`"pending"`** — `sdk/chains/ethereum/src/native/rpc/accounts.rs:50`, block param `:35` | `None` (`wallet.rs:341`) | No |
| Solana | `getBalance` `{"commitment":"finalized"}` — `sdk/chains/solana/src/native/rpc/accounts.rs:148-149` | `None` (`wallet.rs:308`) | No |
| Solana (SPL) | `getTokenAccountsByOwner` filtered by mint, finalized, summed over the owner's token accounts | `None` | No |

The trait doc states the split as intended design:

```rust
// sdk/wallets/src/wallet.rs:21-25
/// Reads a wallet's balance (UTXO chains from the index, account chains from the node).
pub trait BalanceReader: Send + Sync {
    fn balance<'a>(&'a self) -> FutureResult<'a, Balance>;
}
```

`Balance` carries `{ amount, observed_at: Option<BlockRef> }` (`wallet.rs:12-19`);
the route flattens it to `{ amount, observed_height }`
(`apps/api/src/api/wallet.rs:227-236`, schema `:198-207`, contract
`docs/API.md:105`). So `observed_height` is non-null **only for Bitcoin**, and
the one field a caller could use to reconcile is absent exactly where the node
is being trusted.

The `"pending"` default deserves naming on its own. `at = None` selects
`json!("pending")` (`accounts.rs:35`), so today's Ethereum balance is not even a
canonical-block read: it includes the node's local mempool view. Two honest
nodes will disagree by construction. The owner's objection is not a worry about
a hypothetical; it is a description of the current code.

### 2.2 What the movement model already gives

```rust
// sdk/indexing/src/observation.rs:18-76 (elided)
pub enum ValueMovement {
    Transfer { id: MovementId, asset: AssetId, amount: Decimal, from: Address, to: Address },
    Input    { id, asset, amount, owner: Option<Address> },   // spent UTXO
    Output   { id, asset, amount, owner: Option<Address> },   // created UTXO
    Mint     { id, asset, amount, to: Address },
    Burn     { id, asset, amount, from: Address },
}
```

with `from()` / `to()` projecting each variant onto a debit and a credit endpoint
(`observation.rs:127-145`), and `NetworkFee { asset, amount, payer }`
(`observation.rs:150-157`) held beside the movements rather than among them.

Against the required record — cause, wallet, asset, signed delta, block, stable
order — the gap list is short:

| Required | Present? | Evidence |
|---|---|---|
| Cause = transaction id | Yes | `CanonicalTransaction.transaction_id`, `observation.rs:204` |
| Cause = block, no transaction | **No** | Nothing in `InterpretedBlock` (`block.rs:10-17`) can carry one |
| Wallet (address) | Yes | History is copied under every touched address, `observation.rs:225-241` |
| Asset | Yes | `ValueMovement::asset()`, `observation.rs:93-101`; ERC-20 is `AssetId{chain, "0x…"}`, `sdk/chains/ethereum/src/identity.rs:63-75` |
| Signed delta | **Derived, not stored** | Amount is unsigned by invariant; see below |
| Block reference | Yes | `CanonicalStatus::Included { block }`, `observation.rs:184` |
| Stable ordering key | Yes | `(height, transaction_id, ordinal)`; §2.3 |
| Movement for sender **and** receiver when both are ours | Yes | Two address copies, each folding independently |

**Signed deltas must stay derived.** Amounts are non-negative by a validated
invariant at three layers: `Decimal::validate_amount` rejects `Sign::Minus`
(`packages/decimal/src/lib.rs:74-79`), `ObservationDraft::validate_movements`
calls it per movement (`observation.rs:355`), redb re-checks on decode with
`"stored monetary amount must not be negative"`
(`sdk/indexing/redb/src/amount_record.rs:20-22`). Storing a second, signed copy
would create a field that can disagree with the first. The sign belongs to the
`(movement, address)` pair, not to the movement, which is exactly why it is a
projection:

```rust
// proposed: sdk/indexing/src/balance.rs
/// How `movement` moves `address`: a credit, a debit, or nothing.
fn effect(movement: &ValueMovement, address: &Address) -> Effect {
    match (movement.to() == Some(address), movement.from() == Some(address)) {
        (true, true)  => Effect::None,               // self-transfer nets to zero
        (true, false) => Effect::Credit(movement.amount()),
        (false, true) => Effect::Debit(movement.amount()),
        (false, false) => Effect::None,
    }
}
```

**Fee attribution is present but must not be applied uniformly.** `NetworkFee`
already names a payer on every chain: Ethereum sets it to the sender
(`sdk/chains/ethereum/src/native/block/transaction.rs:58-61`), Solana to the fee
payer, Bitcoin to the single common input address
(`sdk/chains/bitcoin/src/native/block/transaction.rs:308-310`). But for Bitcoin
the fee is **already inside the movements**: the interpreter emits an `Input` for
every resolved input and an `Output` for every output
(`native/block/transaction.rs:80-103`), so `Σ our outputs − Σ our inputs` already
nets the fee out. Subtracting `NetworkFee` again on Bitcoin double-charges it.
This is the single most likely bug in a naive implementation and the reason the
fold takes a chain-declared rule rather than guessing:

| Chain | Fee settlement | Fold applies `NetworkFee` as a debit? |
|---|---|---|
| Bitcoin | Implicit — `Σ inputs − Σ outputs` | **No** |
| Ethereum | Explicit — charged outside `tx.value` | Yes, to `fee.payer`, native asset only |
| Solana | Explicit — charged outside the System transfer | Yes, to `fee.payer`, native asset only |

And for ERC-20 specifically: gas is charged in ether, so it is the **native**
wallet's debit, never the token wallet's. `HistoryQuery` already enforces exactly
this — a successful transaction reaches a page on fee grounds only when
`asset == fee.asset` (`sdk/indexing/src/history.rs:99-105`). The fold inherits
the property for free; it must not add its own fee term for a non-native asset.

### 2.3 Ordering is already reorg-stable

`(height, transaction_id)` is the history position
(`sdk/indexing/src/history.rs:11-16`), ordinal within a transaction is the
movement's array index (Postgres `ordinal`, `migrations/0001_init.sql:96`;
redb nests movements in the row blob, `sdk/indexing/redb/src/record.rs:109-115`).
Every component is a function of canonical chain state, not of insertion order,
so after a reorg re-indexes a block the key is deterministic for the new
canonical chain. That is the right property and nothing needs to change.

The redb key even makes the depth cutoff a native range bound:

```
FORMAT(1) ‖ HISTORY(3) ‖ u64be(len)‖chain ‖ u64be(len)‖network
          ‖ u64be(len)‖address ‖ height.to_be_bytes() ‖ u64be(len)‖txid
// sdk/indexing/redb/src/keys.rs:30-50, framing at :93-103
```

Because the address is length-framed, the eight height bytes sit at a fixed
offset within one address's prefix, so keys sort by height ascending. A fold up
to `height <= cutoff` is one ordered prefix scan that stops early.

## 3. Per-block extraction parity: only Ethereum needs another call

This is the table that explains the whole design. Each chain is asked the same
question — *from the data this codebase already fetches for a block, can it
compute every watched address's net change, including changes caused by
contracts?*

| Chain | What a block gives | Contract-initiated effects included? | Extra call needed | Verdict |
|---|---|---|---|---|
| **Bitcoin** | Every input and output with resolved prevout values (`getblock` verbosity 2 + `getrawtransaction` prevout resolution, `sdk/chains/bitcoin/src/native/prevout.rs:51-68`) | **N/A, and complete.** A UTXO chain has no contract-initiated value: value moves only by spending an output | None | **Complete today** |
| **Solana** | `meta.preBalances` / `postBalances` per account key (`sdk/chains/solana/src/native/block/wire.rs:81-82`) | **Yes, by construction.** Pre/post is the runtime's own accounting; a CPI lamport move is already in it | None | **Complete data, wrong use** — §4.2 |
| **Solana (SPL)** | `meta.preTokenBalances` / `postTokenBalances`, each entry naming `{accountIndex, mint, owner, uiTokenAmount}` — the same `getBlock` payload, now decoded into `native::block::TokenBalance` | **Yes, by construction**, for the same reason: it is the runtime's own token accounting, and the entry names the *owner*, so no associated-token address has to be tracked | None | **Closed** — see the SPL note below |
| **Ethereum** | `eth_getBlockByNumber` (full) + `eth_getBlockReceipts` + `debug_traceBlockByHash` (`sdk/chains/ethereum/src/native/rpc/blocks.rs`) | **Yes, since §6 landed.** The prestate diff is the runtime's own accounting; `tx.value` and logs attribute over it | `debug_traceBlockByHash`, one per non-empty block | **Closed** — §6 |
| **Ethereum (ERC-20)** | `Transfer` logs in those same receipts | **Yes.** A log is emitted wherever the transfer originates | None | **Complete for standard tokens** — §7.2 |

Ethereum is the only chain that needs an extra call, and the reason is
structural: Bitcoin's block *is* the state transition, Solana's block *carries*
the state transition, Ethereum's block carries only the *instructions* and the
resulting state transition is computed by execution. That is why the fix is a
trace and why no amount of cleverness with `eth_getBlockByNumber` substitutes
for it.

For ERC-20 the parity already holds and is worth stating explicitly because it
answers the owner's fourth point directly: the log reader is driven by the log,
not by the transaction sender. `Transaction::movements()` maps
`self.receipt.logs.iter().filter_map(|log| log.movement(&self.id))`
(`native/block/transaction.rs:122-126`), and `Log::movement` takes the token from
the log's own `address` field (`native/erc20.rs:110`). A transfer a contract
makes on the wallet's behalf — a DEX router moving the wallet's USDC, a
paymaster, a multisig execution — emits a `Transfer` log with the wallet as an
endpoint and is picked up with no special handling. Admission is
whole-transaction: if any endpoint of any movement matches a watched address, the
entire draft with all movements of all assets is emitted
(`sdk/chains/ethereum/src/interpreter.rs:119-130`), so a token arriving at a
watched address is indexed even though the token was never registered.

## 4. Changes with no transaction to name

### 4.1 Ethereum: consensus-layer withdrawals — visible in bytes already held

Post-Shanghai, a validator's partial withdrawals and full exits credit an
execution-layer address directly in the block body, with **no transaction**. The
indexer produces nothing for them: `grep -rn -i withdraw sdk/chains/ethereum/`
returns zero hits.

But the data is already in the process. `Head::parse` deserialises the whole
block through alloy:

```rust
// sdk/chains/ethereum/src/native/block/head.rs:48
let block: RpcBlock<Value> = serde_json::from_slice(raw)...
```

and `alloy_rpc_types_eth::Block` carries `pub withdrawals: Option<Withdrawals>`
(`alloy-rpc-types-eth-1.8.3/src/block.rs:42`; version pinned at
`Cargo.lock:208-210`). `Head` keeps only `{ reference, transactions }`
(`head.rs:21-24`), so the field is deserialised and dropped when `block` falls out
of scope at `head.rs:73`. The raw bytes survive inside `RawBlock.block`
(`native/block/mod.rs:34`) for the whole pass.

**Verdict: block-level cause record. Zero new RPC calls, zero change to the
evidence shape, one field on `Head`.** This is the cheapest correctness win in
the document.

Two traps to write into the implementation:

- **`amount` is in Gwei, not Wei.** The withdrawal object's `amount` is
  `uint64` Gwei per the Engine API. Multiply by 10^9 before building a `Wei`.
  A missed conversion under-reports by a factor of a billion and will not look
  obviously wrong.
- **Withdrawals credit the address unconditionally**, including addresses with
  no code and no transaction history. An address that has only ever received
  withdrawals has an empty transaction history and a non-zero balance — which is
  precisely the zero-balance trap of §7.1 in its most concrete form.

Consequence if excluded instead: any watched address that is a validator
withdrawal target drifts permanently upward against its derived balance, and
the drift is silent because nothing reconciles.

### 4.2 Solana: the guard that halts instead of recording

Solana's interpreter decodes **only System-program transfers**
(`sdk/chains/solana/src/native/block/instruction.rs:51-97`; everything that is
not `SystemInstruction::Transfer` or `TransferWithSeed` returns `None` at `:90`).
It then checks the result against the runtime's own accounting:

```rust
// sdk/chains/solana/src/native/block/transaction.rs:131-143
for key in affected {
    let actual = i128::from(key.balance.after.units()) - i128::from(key.balance.before.units());
    let mut expected = self.delta(&key.address);
    if key.address == *self.payer() { expected -= i128::from(self.fee.amount.units()); }
    if actual != expected {
        return Err(Error::invalid_block(
            "selected Solana account has an unsupported native SOL value effect",
        ));
    }
}
```

This is reached from `interpreter.rs:105-107`, and `invalid_block` is
**terminal**. So the moment a watched Solana account is touched by any lamport
effect that is not a plain System transfer — a CPI moving lamports, an account
close returning rent, a `CreateAccount` funding — indexing for that network stops
on that block. Not wrong data: no data, and a halt.

The fix is not to add more instruction decoders. The authority is sitting right
there in `key.balance.after - key.balance.before`. Invert the guard: where the
decoded System transfers do not explain the delta, emit the **residual** as an
attributed movement rather than rejecting the block. The cause is known (this
transaction), the amount is exact, the account is known. It satisfies the owner's
requirement — *"track what changed in each block, that is including the smart
contract calls"* — exactly, and it is the same shape as the Ethereum trace fix,
with the data already in hand.

The reconciliation check does not disappear; it becomes total. After emitting
the residual, `actual == expected` holds by construction for every watched
account, and the guard degrades to an assertion that the arithmetic is sound.

On the two commonly cited Solana block-level causes, the honest answer is that
neither is the real problem:

| Cause | Reality | Verdict |
|---|---|---|
| Epoch / staking rewards | Credited to **stake and vote accounts**, not to ordinary SOL wallets. This SDK manages neither. Carried in the block-level `rewards` array, which the source explicitly declines: `"rewards": false` (`sdk/chains/solana/src/native/rpc/blocks.rs:129`) and which `Reply` has no field for (`native/block/wire.rs:9-20`) | **Documented exclusion.** Flip `rewards: true` and parse it only if stake/vote accounts ever become watchable — a one-line request change that enriches the `getBlock` payload without altering `RawBlock { reference, bytes }` (`native/block/mod.rs:34-37`) |
| Rent collection | Rent-paying accounts are deprecated; all accounts are rent-exempt. Nothing in the repo handles rent and nothing needs to | **Documented exclusion** |
| Non-System CPI lamport movement | Real, common, and today a **terminal halt** | **Fix — §4.2 above** |

#### 4.2.1 SPL tokens: the same authority, one shape difference

*Added when SPL tokens landed; it does not change anything above.*

`meta.preTokenBalances` / `postTokenBalances` travel in the very `getBlock`
payload this repo already fetches (`native/rpc/blocks.rs`), and they are the
runtime's own token accounting, exactly as `pre/postBalances` are for lamports.
They were simply dropped, because `native/block/wire.rs` had no field for them;
the fields and `native::block::TokenBalance` now decode them, and a USDC
transfer to a watched address is indexed with its mint, amount, and transaction
id.

The shape differs from Ethereum's in one way worth stating plainly. An ERC-20
`Transfer` log names **both** endpoints, so every ERC-20 movement can be a
`Transfer`. A token-balance diff names **one** side — a position — so a diff
alone yields one-sided `Mint`/`Burn` movements per owner, which would report an
ordinary wallet-to-wallet USDC payment as a burn beside a mint. That is wrong
data, not merely coarse data, so SPL `Transfer` and `TransferChecked`
instructions are decoded to recover both owners (via the `owner` field of the
two token accounts' entries), and the balance diff remains the authority:
anything undecoded — a mint, a burn, a Token-2022 transfer fee, a program this
crate does not read, a node that reported no token balances at all — falls out
as a one-sided residual with a `:token:<account index>` id. That is precisely
how the lamport path above now works, and `reconcile` asserts the same identity
per `(owner, mint)` that it asserts per account.

Three absences are data, never a halt — the class of defect §4.2 removed two
terminal guards for, and no third was added:

| Absence | Meaning | Handling |
|---|---|---|
| No `pre` entry for a position | The token account was created in this transaction | Before = 0 |
| No `post` entry | The token account was closed in this transaction | After = 0 |
| `preTokenBalances` / `postTokenBalances` null or absent, an entry with no `owner`, or an amount the program does not expose as a plain count | The node or the program did not record it | The position is not attributed; the block still indexes |

Decimals are **not** taken from configuration. `RpcClient::validate_token`
reads the mint account (`getAccountInfo`, finalized) at startup and compares
its `decimals` byte with the configured value; the token-balance `decimals`
field was not used for this, because it only exists once some transaction has
already touched the mint, which is not a precondition a deployment can rely on.
The same read is the refusal path: a Token-2022 mint is refused with
`Unsupported`, because a confidential transfer need not report the amount this
document requires, and indexing it as zero would be exactly the silent-wrong
answer this document exists to prevent.

### 4.3 Bitcoin: none, confirmed

Every satoshi that reaches an address does so by being an output of a
transaction, coinbase included. The coinbase is a real transaction with a real
id; its input carries `previous: None`
(`sdk/chains/bitcoin/src/native/block/transaction.rs:224-232`) so it emits no
`Input` movement, only reward `Output`s, and the maturity rule is applied at read
time against `COINBASE_MATURITY = 100`
(`sdk/chains/bitcoin/src/sender/utxos.rs:172`,
`native/block/output.rs:12`). There is no block-level credit, no fee rebate, no
consensus-layer transfer. Bitcoin needs no block-level cause record.

One nuance the fold must preserve: today's balance excludes immature coinbase
outputs because it is a *spendable* balance. A fold over history has no maturity
concept. Either the fold learns the rule or the two numbers legitimately differ
for a mining address. Recommendation: the fold reports the **economic** balance
and a separate `spendable` figure stays with the UTXO reader, which is what the
sender needs anyway.

### 4.4 The record shape for a cause with no transaction

Two ways to carry it, and the cheap one is a type lie:

**(a) Synthetic transaction id — rejected.** Encode the cause as a
`TransactionRef` with id `block:{height}:withdrawal:{index}`. It needs no
migration at all: history rows, the fold, pagination and the reorg journal all
work unchanged. It is rejected because `TransactionRef` means "the chain's
transaction identifier" (`observation.rs:204`), chain crates produce it only via
`TransactionId::canonical()` (`docs/CHAIN_LAYOUT.md:88-114`), and the owner asked
for these to be *visibly* transactionless. A reserved string prefix that every
reader must know about is worse than a field.

**(b) A cause discriminator — recommended.** `InterpretedBlock` grows a third
collection beside `transactions` and `outputs`:

```rust
// sdk/indexing/src/block.rs:10-17, extended
pub struct InterpretedBlock {
    pub block: BlockRef,
    pub transactions: Vec<ObservationDraft>,
    pub outputs: OutputChanges,
    pub credits: Vec<BlockCredit>,   // new: causes with no transaction
}

/// One value change a block makes with no transaction to name it.
pub struct BlockCredit {
    pub id: MovementId,      // deterministic from canonical data, e.g. "withdrawal:3"
    pub asset: AssetId,
    pub amount: Decimal,
    pub to: Address,
}
```

`BlockCredit` is credit-only on purpose: no chain the SDK supports debits an
address without a transaction, and a signed block-level record would be a
speculative generalisation. If one ever appears, it earns a `from` field then.

Storage cost is stated in full in §8.3. It is one additive Postgres migration and
one new redb key tag; no existing row changes.

## 5. `balance(wallet, confirmations = X)`

### 5.1 Definition

Confirmations are already defined once, at read time, and nothing stores them
(`sdk/indexing/src/indexer.rs:99-110`, invariant documented at
`observation.rs:179-180`):

```
confirmations(block) = checkpoint.height − block.height + 1
```

So *"at least X confirmations"* is `block.height <= checkpoint.height + 1 − X`,
and the balance is a fold over everything at or below that cutoff:

```
cutoff(X)  = checkpoint.height + 1 − X                    // X = 1 includes the tip
balance(W = (address, asset), X)
  = Σ credits(address, asset, height ≤ cutoff)
  − Σ debits (address, asset, height ≤ cutoff)
  − Σ fees where fee.payer = address ∧ fee.asset = asset   // account chains only, §2.2
```

`X` defaults to the network's `SyncPolicy::minimum_confirmations`
(`sdk/indexing/src/sync_policy.rs:11`), the same number history already uses to
say `Confirmed` — so the two endpoints agree by construction instead of by
coincidence. A caller may pass a larger `X`; `X = 0` is rejected the way the
policy already rejects it (`sync_policy.rs:36-39`).

Failed transactions contribute nothing but their fee, which falls out for free:
draft validation forbids movements on a failed transaction
(`observation.rs:341-349`), while the fee survives.

### 5.2 The arithmetic problem, and why it is a feature

`Decimal` has no arithmetic, deliberately:

> "Arithmetic is deliberately absent. This type carries amounts between a chain,
> storage, and an HTTP boundary; it never adds, divides, or rounds them."
> — `packages/decimal/src/lib.rs:19-22`

That rule should not be relaxed for this. Movements are stored as exact atomic
units at scale 0 — every interpreter calls `.atomic()`
(`sdk/chains/ethereum/src/interpreter.rs:61`, and Bitcoin's test asserts
`fee.amount.scale() == 0` at `interpreter.rs:394`) — so the fold can run in
`BigUint` through the existing, public `Decimal::to_atomic(0)` /
`Decimal::from_atomic(units, 0)` pair (`packages/decimal/src/lib.rs:52`,
`:87`) and never touch `Decimal` arithmetic:

```rust
let (mut credits, mut debits) = (BigUint::zero(), BigUint::zero());
// ... accumulate per §2.2 effect() ...
match credits.checked_sub(&debits) {
    Some(units) => Balance::complete(Decimal::from_atomic(units, 0)),
    None        => Balance::incomplete(),   // provably missing credits
}
```

The failure branch is the useful part. A correct, complete index can never spend
more than it received, so **a fold that goes negative is a proof that the index
is incomplete** — the cheapest possible self-check, and precisely the alarm that
today's Ethereum silently lacks. It fires on exactly the cases §6 is about: an
address funded by an internal transfer and spending normally.

Because the fold works in atomic units, it is **independent of `decimals`**.
Token decimals are supplied in config and verified against the contract at
startup (`Client::validate_token`, `sdk/chains/ethereum/src/native/rpc/accounts.rs:90-129`,
mismatch is terminal at `:112-120`; stored on `WalletConfig.decimals`,
`sdk/chains/ethereum/src/wallet.rs:35`), and the indexer never learns them at
all. Scaling happens once, at the edge, exactly where it happens now
(`decimal_with(self.config.decimals)`, `wallet.rs:339`). A whole class of
decimals bugs cannot occur inside the fold.

### 5.3 Store-side, not read-side

**Read-side folding of history pages is rejected.** A history cursor binds to the
first page's checkpoint and a page at a different checkpoint is a retryable
`Conflict` (`sdk/indexing/src/indexer.rs:44-52`; Postgres proves it by re-reading
the checkpoint inside the transaction, `sdk/indexing/postgres/src/read.rs:83-96`).
Folding an address's whole history therefore aborts whenever a block commits
mid-fold — on Solana, most of the time. It is also 4 round trips per page
(`read.rs:3-5`) for a number the store can compute in one.

So: a new port beside the three that exist, one method, within the linter's
`trait-method-count` limit of 3 (`linter.toml:118-213`):

```rust
// sdk/indexing/src/balance.rs  (new file — observation.rs is 475 lines
// against the 500-line cap at linter.toml:70-72, so this cannot live there)
pub struct BalanceQuery { pub address: Address, pub asset: AssetId, pub confirmations: u64 }

pub struct BalanceAt {
    pub amount: Decimal,
    pub as_of: BlockRef,          // the cutoff block, not the checkpoint
    pub checkpoint: BlockRef,
    pub confirmations: u64,
    pub completeness: Completeness,
}

pub trait BalanceStore: Send + Sync {
    fn balance<'a>(&'a self, query: BalanceQuery) -> BoxFuture<'a, Result<BalanceAt, IndexError>>;
}
```

| Backend | Implementation | What it needs |
|---|---|---|
| redb | Ordered prefix scan over `history_prefix(network, address)` (`keys.rs:30-34`), stopping at the first key whose height exceeds the cutoff; decode each `TransactionRecord`, apply `effect()` per movement of the asset | **Nothing new.** Height is already in the key at a fixed offset (`keys.rs:47`), so the scan terminates early instead of reading the whole address |
| Postgres | One aggregate over `payments_movement` plus one fee term from `payments_history` | One **additive** index; §8.3 |

The Postgres shape, mirroring the `HISTORY_PAGE` predicate
(`sdk/indexing/postgres/src/read.rs:26-39`) so the two endpoints cannot drift:

```sql
SELECT
  COALESCE(SUM(CASE WHEN m.to_address   = $3 THEN m.amount ELSE 0 END), 0)::text AS credits,
  COALESCE(SUM(CASE WHEN m.from_address = $3 THEN m.amount ELSE 0 END), 0)::text AS debits
FROM payments_movement m
WHERE m.chain = $1 AND m.network = $2 AND m.address = $3
  AND m.asset_chain = $4 AND m.asset = $5
  AND m.height <= $6;
```

A self-transfer appears in both sums and cancels, matching `effect()`. The fee
term is a second, trivial aggregate over `payments_history` filtered on
`fee_payer = $3 AND fee_asset = $5`, applied only for the chains of §2.2.

Both run inside the same `RepeatableRead`, `read_only` transaction the history
page already uses (`read.rs:74-80`) with the same checkpoint re-read
(`read.rs:83-96`), so the returned `as_of` is provably the checkpoint the sums
were taken under. redb gets the equivalent from its snapshot-consistent scan.

Both backends run a new `indexing::testing::BalanceContract` beside the existing
`HistoryContract` / `RegistryContract`
(`sdk/indexing/src/testing/history.rs:1-20`) — the repository-contract convention
`AGENTS.md:412` already requires for both backends.

### 5.4 A stored running total is not worth it yet

| | Fold per query | Stored running total |
|---|---|---|
| Reorg | Free — rows are deleted by predicate (`sdk/indexing/postgres/src/revert.rs:21-22`) or by journalled key (`sdk/indexing/redb/src/repository/blocks.rs:292-313`) and the sum changes with them | The journal must carry a per-wallet balance delta to unwind, growing `JournalRecord` (`record.rs:134-141`) and the Postgres journal tables |
| Correctness | Cannot disagree with the rows; it *is* the rows | A denormalisation that can silently diverge — the exact failure mode this document exists to remove |
| Cost | One indexed aggregate; sub-millisecond below ~10^5 movements per address | O(1) read, O(writes) maintenance on the hot commit path |

**Defer.** Revisit when a single watched address exceeds roughly 10^6 movements,
and then as a *checkpointed* total — balance at every Nth height, stored only
below the reorg-retention window where rows are immutable, plus a fold of the
remainder. That design is reorg-safe by construction, which the naive running
total is not. Nothing should be built for it now.

Pagination does not arise: a balance is one aggregate, not a page. The
`Incomplete` detection of §5.2 needs only the two totals, not an ordered walk.

## 6. Ethereum internal transfers

The requirement is now firm — contract-initiated ETH movement must be seen — so
this is a costing exercise, not a comparison of options.

### 6.1 What is missed today

```rust
// sdk/chains/ethereum/src/native/block/transaction.rs:105-128
pub fn movements(&self) -> Vec<Movement> {
    if !self.succeeded() { return Vec::new(); }
    let native = self.to.as_ref()
        .or(self.receipt.contract_address.as_ref())
        .filter(|_| !self.value.is_zero())
        .map(|to| Movement { id: format!("{}:value", self.id), ... });
    let tokens = self.receipt.logs.iter().filter_map(|log| log.movement(&self.id));
    native.into_iter().chain(tokens).collect()
}
```

Exactly two producers: the top-level `tx.value`, and `Transfer` logs. Therefore
invisible: ETH forwarded by a router or splitter contract, a withdrawal from an
exchange proxy or an escrow, a multisig or smart-account execution, a
`SELFDESTRUCT` beneficiary credit (`selfdestruct` appears nowhere in the repo),
and the value leg of any `CALL` a contract makes. For a wallet that receives ETH
through any contract, the derived balance is simply wrong, and today nothing
notices.

### 6.2 Which call

| Method | Namespace | Shape | Serves it |
|---|---|---|---|
| `debug_traceBlockByNumber` / `ByHash`, `{"tracer":"callTracer"}` | `debug_` (geth-family) | Nested call tree per transaction; `CALL`/`CREATE`/`SELFDESTRUCT` frames with `value` | geth, Reth, Erigon, Nethermind — i.e. every major client. Widely exposed by managed providers on paid tiers |
| `debug_traceBlockByNumber`, `{"tracer":"prestateTracer","tracerConfig":{"diffMode":true}}` | `debug_` | Per-transaction pre/post state **including balances** | Same clients |
| `trace_block` | `trace_` (OpenEthereum lineage) | Flat trace list for the whole block | Erigon, Nethermind, Reth with the `trace` namespace. **Not geth** |

**Decided and built: `debug_traceBlockByHash` with `prestateTracer` in
`diffMode`.**

The reasoning is the symmetry of §3. `prestateTracer` returns, per transaction,
the balance before and after for every touched account — structurally identical
to Solana's `preBalances`/`postBalances`, which §4.2 makes the authority. Both
chains then use the same rule: *the runtime's own accounting is the truth; the
decoded instructions are a cross-check.* One mechanism, one mental model, one
class of bug. `callTracer` instead requires the interpreter to re-derive net
effects by walking a call tree and reasoning about which frames reverted —
`callTracer` reports reverted subcalls with an `error` field, and a mistake there
is a silent wrong balance.

`ByHash` rather than `ByNumber` because the source already holds the canonical
hash and re-verifies it around interpretation
(`sdk/indexing/src/source.rs:37-40`), so pinning by hash removes a reorg race for
free.

Two traps to write into the implementation, and how the built code answers
them:

- **`prestateTracer` balance diffs already include the gas payment** (and, for
  the coinbase account, the priority fee). Subtracting `NetworkFee` again
  double-charges — the same class of bug as Bitcoin's fee in §2.2, in a second
  place. **As built**, the fee is added back on the credit side when a residual
  is computed, exactly as Solana's `unexplained` already does
  (`sdk/chains/solana/src/native/block/transaction.rs`). So a sender's residual
  for a plain transfer is zero, `NetworkFee` stays the one record of gas, and
  the fold needs no per-chain fee rule for native ETH. Pinned by
  `a_plain_transfer_is_counted_once_and_its_gas_is_never_charged_twice`.
- A traced block must still reconcile against the **top-level** `tx.value` the
  current parser produces; keep that as the assertion, not as a second source of
  movements, or every plain transfer is counted twice. **As built**, `tx.value`
  remains the only producer of the `<txid>:value` movement and the trace is
  subtracted against it rather than read for movements of its own: a residual is
  by definition what `value` and the fee leave over, so a plain transfer yields
  exactly one movement. The trace's own identity is checked against the block
  by count, position, and `txHash`. Pinned by the same test and by
  `trace_evidence_must_describe_this_block_and_be_a_diff`.

One case the original text did not name: an account the `value` movement credits
can be **absent from the diff** — a router that forwards on everything it was
sent ends where it started. It is settled anyway, which produces the debit that
cancels the attributed credit. Without that, a watched router would accumulate a
phantom balance.

`trace_block` stays as a documented alternative for `trace_`-namespace
deployments, selected by the same capability probe as §6.5. It is a second code
path and should not be built until a deployment needs it.

### 6.3 Cost, honestly

Order-of-magnitude figures, to be re-measured against the actual provider before
committing — this is the kind of number that changes:

| | Today | With a block trace |
|---|---|---|
| Calls per block | 2 (`eth_getBlockByNumber` + `eth_getBlockReceipts`, `blocks.rs:94`, `:123`) | 3 |
| Payload per mainnet block | ~10^5–10^6 bytes | ~10^6–10^7 bytes — roughly an order of magnitude |
| Provider billing weight | 1 unit | commonly 50–300× a plain call |
| Latency | ~10^2 ms | ~10^2–10^3 ms; a heavy block can be seconds |

The decisive constraint is not price, it is **state availability**. A non-archive
node can trace only blocks within its state-retention window (geth's default is
about 128 blocks of in-memory state, plus whatever it can regenerate). Tracing a
block from a year ago requires archive state. This is not a tuning detail — it
sets the backfill horizon for Ethereum directly, and §7.6 uses it.

The block-fetch path is otherwise unaffected: `json_rpc` already has batching
(`packages/json-rpc/src/client.rs:35-38`, `http.rs:320`) and the receipts path
already negotiates capability once and caches it in an atomic
(`blocks.rs:100-118`), so the trace probe has a pattern to copy.

### 6.4 Evidence shape: a real change, with smaller blast radius than it looks

```rust
// sdk/chains/ethereum/src/native/block/mod.rs:31-47
pub struct RawBlock { reference: BlockRef, block: Vec<u8>, receipts: Vec<Vec<u8>> }
impl RawBlock { pub const fn new(reference: BlockRef, block: Vec<u8>, receipts: Vec<Vec<u8>>) -> Self }
```

This gained a `traces: Vec<u8>` field and `new` a fourth parameter, with a
`traces()` accessor beside `block()` and `receipts()`. That is a **breaking
change to a public type and constructor in the Ethereum chain crate**, and it
must be flagged as such. It was verified before the change that nothing
persists a `RawBlock`: the only constructors are `native::rpc::blocks` and test
fixtures, and no store, codec, or fixture file holds one. But the scope is narrower than "replay format
change", and the distinction matters:

- **Nothing persists `RawBlock`.** The synchronizer fetches `I::Block`, calls
  `inspect`, stores only the interpreted result via `BlockAddition::new`
  (`sdk/indexing/src/synchronizer.rs:288-320`), and drops the raw block at the
  end of the pass. `BlockAddition` holds `Vec<CanonicalTransaction>` and
  `OutputChanges` (`sdk/indexing/src/block.rs:54-61`) — no bytes. There is no
  fixture directory and no serialised `RawBlock` on disk anywhere in the repo.
- So there is **no stored replay format to migrate, no archived block to
  rewrite, and no backward-compatibility surface**. The change is source-level:
  `RawBlock`, `Head`, the `Source` construction site, and the interpreter.
  `AGENTS.md:32-45` explicitly permits replacing unpublished internal types
  directly, with no aliases and no `V1`/`V2`.
- Traces are retained **in the in-memory `RawBlock` for the duration of one
  pass**, because `BlockInterpreter::inspect` is pure and synchronous and may not
  touch the network (`sdk/indexing/src/source.rs:45-46`) — so whatever it reads
  must arrive with the block. They must **not** be written to storage:
  `AGENTS.md:352-356` forbids raw-block archives outright, and nothing needs one.
- **Bitcoin's and Solana's evidence shapes do not change.** Bitcoin keeps
  `getblock` verbosity 2 plus resolved prevouts; Solana keeps
  `RawBlock { reference, bytes }` (`sdk/chains/solana/src/native/block/mod.rs:34-37`)
  — and note that even the `rewards: true` flip of §4.2 changes only the *content*
  of those bytes, not the struct.

Layout constraint worth naming now: `linter.toml:268-293` makes
`sdk/chains/*/src` a restrictive directory with a fixed file list, so trace
decoding cannot become `sdk/chains/ethereum/src/trace.rs`. It belongs under
`native/` — `native/block/trace.rs` beside `receipt.rs` and `log.rs`
(`docs/CHAIN_LAYOUT.md:53-57`).

### 6.5 If the node cannot trace — decided: it is refused, with no opt-out

**Status: implemented.** The repository owner rejected the opt-out this section
originally recommended. A node that cannot trace is not a supported Ethereum
node, and there is no configuration that makes one acceptable.

| Option | Verdict |
|---|---|
| Refuse to index Ethereum at all | **Adopted.** Contract-initiated ether is ordinary on Ethereum, not an exotic case a deployment can opt out of; an ERC-20-only deployment is not a reason to ship a wrong ETH balance |
| Index with a silent gap | **Rejected.** "Reject unsupported meaningful input; never accept ignored configuration" (`AGENTS.md:400`), the rule `019` §6.2 already invoked to reject warn-and-continue |
| Per-network capability probe with an explicit opt-out | **Rejected by the owner.** `"native_balance": "transactions_only"` and a per-wallet `completeness: "transactions_only"` are not built. A degraded label is a way to ship a wrong number with paperwork attached |

Concretely, as built: `Config::connect` verifies chain id, then calls
`RpcClient::validate_tracing`, then every token's code and decimals
(`apps/server/src/chains/ethereum.rs`). The probe traces the node's *own head*
rather than asking whether the method exists, because a node that answers
`debug_traceBlockByHash` while ignoring `diffMode` returns a bare prestate,
which would read as every touched account being emptied. Failure is terminal
and names the method, the tracer, and the fix.

The composition with `019` is unchanged and is the reason refusing is safe:
Ethereum's section fails on its own, Bitcoin and Solana compose and index
normally. If an endpoint loses the capability after startup — a failover to a
node without the `debug` namespace — the per-block error is
`RpcUnavailable`/non-retryable, which `019` turns into a quarantine of
`ethereum/<network>` alone.

`trace_block` remains the documented alternative for `trace_`-namespace-only
deployments. It is a second code path and is still not built.

## 7. Completeness without a node balance

### 7.1 The invariant, and the trap in it

For a wallet `W = (address, asset)` with birthday `B` and checkpoint `C`:

> `balance(W, X)` equals `W`'s true on-chain balance at `cutoff(X)` **iff**
> (i) every block in `[B, C]` was inspected for `address`, **and**
> (ii) every balance-affecting effect within each such block was extracted, **and**
> (iii) `W`'s true balance at `B − 1` was zero.

(i) is knowable: it is checkpoint continuity from `B`, which the synchronizer
guarantees by construction. (ii) is what §4 and §6 are about, and is now
chain-decidable. **(iii) is the trap, and it is not knowable from the index at
all.**

An address with no indexed history folds to zero and therefore "balances". That
satisfies the invariant vacuously and says nothing: it is indistinguishable from
an address that was funded before `B`. Completeness is a claim about `B`, never a
property of the rows. Any design that reports `complete` because the fold
succeeded is lying.

### 7.2 What breaks a derived ERC-20 balance

Folding `Transfer` logs gives the true token balance only if every balance change
emits one. It usually does, and the cases where it does not divide cleanly into
"we are fine" and "we are silently wrong":

| Case | Effect on the fold | Detectable? |
|---|---|---|
| Fee-on-transfer / deflationary | **Correct.** Such tokens emit a second `Transfer` to the fee sink, and the reader takes every log in the receipt (`transaction.rs:122-126`), not just the one the sender intended | N/A |
| Mint / burn via the zero address | **Correct.** Decoded as `Mint`/`Burn`, zero endpoint dropped (`native/erc20.rs:101-113`) | N/A |
| Several tokens at one address | **Correct.** Asset is part of the wallet key and of every query | N/A |
| Self-transfer | **Correct.** Credit and debit cancel (§2.2) | N/A |
| Non-canonical log encoding (wrong topic count, non-zero address padding, wrong data width) | Movement **silently dropped** — `Log::movement` returns `None` (`native/erc20.rs:89-98`) | Only by the negative-fold check of §5.2, and only if it causes an overdraw |
| **Rebasing / elastic supply** (stETH, AMPL, OHM-likes) | **Silently wrong.** Balance changes with no `Transfer` at all; the fold tracks share transfers while the contract reports rebased units | Only against `balanceOf` |
| **Mint/burn not spelled as a `Transfer`** | **Silently wrong** | Only against `balanceOf` |
| **No `Transfer` event at all** (non-standard token) | **Silently wrong**, and the wallet never sees incoming funds either | Only against `balanceOf` |
| **Proxy upgrade changing accounting mid-life** | **Silently wrong from the upgrade block**, undetectable from logs | Only against `balanceOf` |

The bottom four have one thing in common: they are invisible in logs by
construction, so no amount of log-reading finds them. The only cross-check is
`balanceOf`.

### 7.3 `balanceOf` is not the same kind of claim as `eth_getBalance` — and the difference is real

The owner rejected trusting node state reads. That objection does **not** transfer
to `balanceOf` unchanged, and the distinction should be made explicitly rather
than assumed either way:

| | `eth_getBalance` | `balanceOf` via `eth_call` |
|---|---|---|
| What it is | The node reporting its own account-state accounting | Deterministic EVM execution of contract bytecode |
| Reproducible across nodes? | Only if both are at the same block and both are correct | **Yes, by consensus.** Two nodes at the same block hash that disagree mean one is broken |
| Pinnable | Yes, by block | Yes, by block |

So a `balanceOf` pinned to a canonical block **hash** is a reproducible
computation, not a matter of node opinion. The code can already pin it —
`{"blockHash": …, "requireCanonical": true}`
(`sdk/chains/ethereum/src/native/rpc/accounts.rs:42-45`).

The catch is that the production call site does not use it. `BalanceReader`
passes `at = None` (`sdk/chains/ethereum/src/wallet.rs:336`), which selects
`json!("pending")` (`accounts.rs:35`). At `"pending"` the owner's objection
applies with **full force to both calls**: pending state is the node's local
mempool view and two honest nodes differ routinely.

**Recommendation.** Keep `balanceOf` — and `eth_getBalance` — but only pinned to
the fold's cutoff block hash, and only as a **witness**:

```
GET /v1/wallets/{id}/balance
  amount      <- the fold, always                  (the answer)
  attested    <- true | false | "unavailable"      (the witness agreed, disagreed, or was not asked)
```

This costs one call, never changes the returned number, and turns every silent
failure in §7.2's bottom four rows into a visible `attested: false`. Symmetrically
it does the same for native ETH, which is otherwise the one asset with no
completeness oracle at all (§7.4). It is opt-in per network so a deployment that
refuses to call a node for balance purposes at all can switch it off and get
`"unavailable"`.

This is the one place where a node read earns its keep, and it earns it precisely
because it is not the answer.

### 7.4 What can actually be proven, per chain and per asset

| Asset | Enumeration oracle | Can prove completeness back to? | Verdict |
|---|---|---|---|
| **Bitcoin** | **None.** Core has no address index. The repo uses only `getblock*`, `getblockhash`, `getblockheader`, `getblockcount`, `getrawtransaction`, `getblockchaininfo`, `getindexinfo`, `estimatesmartfee`, `testmempoolaccept`, `sendrawtransaction` — no `scantxoutset`, no wallet RPCs | Nothing. Only a full block scan from `B` | Policy, not proof |
| **Solana** | `getSignaturesForAddress` — **not used anywhere**; explicitly rejected as a sync mechanism in `docs/adr/0026:260-263` | Ledger retention only (`getFirstAvailableBlock`, `sdk/chains/solana/src/native/rpc/blocks.rs:80`), and blind to any effect with no signature | Policy, not proof |
| **Solana (SPL)** | `getTokenAccountsByOwner`, summed — the token analogue of `getBalance`, and the same kind of claim: a node's own view at a contextual slot, not a canonical checkpoint | Same retention bound, plus: a mint whose amounts the program need not expose (Token-2022 confidential transfers) is refused at startup rather than witnessed as zero | Policy, not proof |
| **Ethereum native** | **None** without trusting a balance read | Nothing | Policy, plus the §7.3 witness |
| **Ethereum ERC-20** | **`eth_getLogs`** on the token contract, filtered by the `Transfer` topic with the address in topic 1 or topic 2 | **The token's deployment block** — a genuine proof | **Real backfill primitive** |

That last row is the asymmetry the owner asked about, and it is worth stating
plainly: **a wallet can be complete for its ERC-20 asset and not for native ETH
at the same address.** Since wallets here are per `(address, asset)` — one
registry row per pair, `sdk/indexing/src/registry.rs:15-30`, enforced by
`UNIQUE (chain, network, address, asset)` in `migrations/0002_wallet_asset.sql`
— the model expresses it directly. `completeness` is a per-wallet field, not a
per-address one.

`eth_getLogs` is not used anywhere in the repo today (zero hits). Adding it is a
new call shape for the Ethereum crate; `docs/CHAIN_LAYOUT.md:107` pins `Source`
as the indexer's block source and nothing else, so an ERC-20 backfill belongs in
its own `native/rpc` reader driving the rescan of `019` §5.3, not in `Source`.

Practical bounds on the proof: providers cap `eth_getLogs` by block range, and
the caps vary by an order of magnitude between them (commonly a few thousand
blocks for broad filters; a filter narrowed by both contract address and an
indexed topic is usually permitted over much wider ranges, subject to a response
size cap). A backfill is therefore
`ceil((now − deployment) / range)` requests, which for a token deployed years ago
is thousands of calls — bounded, parallelisable, and cheap per call. That is a
real cost but a *finite, knowable* one, which is exactly what native ETH lacks.

### 7.5 The "too old to accept" rule

`019` §6 established that a birthday below the checkpoint today silently indexes
nothing and must be refused. This document adds *why a birthday might be refused
even with a rescan available*, and where the number comes from.

A birthday `B` is acceptable for network `N` iff all three hold:

| Bound | Source | Fails when |
|---|---|---|
| **Data availability** | Bitcoin: unpruned node (already required, `sdk/chains/bitcoin/src/native/rpc/node.rs:67-72`). Solana: `getFirstAvailableBlock` (`blocks.rs:80`). Ethereum: the node can trace at `B` — i.e. archive state, §6.3 | The node simply cannot answer for `B`. **Hard refusal** — no configuration overrides physics |
| **Scan cost** | `(C − B)` blocks × per-block cost. Bitcoin additionally pays up to `MAX_EXTERNAL_PREVOUTS = 25_000` `getrawtransaction` calls per block (`sdk/chains/bitcoin/src/native/prevout.rs:28`). Ethereum pays the trace multiplier of §6.3 | Exceeds a per-network `max_backfill_blocks` in config. **Refusal with the number in the message** |
| **Reorg retention** | Irrelevant here — retention bounds rewind, not backfill (`019:479-487`: "Rewinding past retention is not a rewind, it is a purge") | Never |

Refusal must say which bound failed and what the earliest acceptable birthday is,
so an operator can act. It follows the shape `019` §6.3 already specifies for the
checkpoint case and shares its `already_registered_at` refinement, so a
correctly configured server does not start failing on every restart.

ERC-20 is the exception that proves the rule: for a token wallet the backfill is
`eth_getLogs` from the token's deployment, which needs no archive state and no
block scan, so a token wallet can accept a birthday that the same address's
native wallet must refuse.

## 8. API and storage

### 8.1 The balance endpoint

Today: `{"amount":"12.5","observed_height":42}` (`docs/API.md:105`,
`apps/api/src/api/wallet.rs:198-207`). Proposed, additive:

```json
{
  "amount": "12.5",
  "observed_height": 842112,
  "as_of_height": 842112,
  "as_of_hash": "0x…",
  "confirmations": 12,
  "checkpoint_height": 842123,
  "completeness": "complete",
  "attested": true
}
```

`observed_height` keeps its name and becomes an alias of `as_of_height`, so the
existing OpenAPI contract test (`apps/api/tests/openapi_contract.rs`) needs
updating but no client breaks — and for the first time it is non-null on all
three chains, not only Bitcoin.

`completeness` is a closed vocabulary, which `linter.toml`'s
`string-backed-finite-state` rule requires be an enum at three or more variants:

| Value | Meaning |
|---|---|
| `complete` | Every change since `B` extracted, and `B` proven at or before first use (today: ERC-20 with a `getLogs` backfill only) |
| `from_birthday` | Every change since `B` extracted; nothing proves `B` early enough. The honest default for Bitcoin, Solana and native ETH |
| ~~`transactions_only`~~ | **Removed.** It existed only for the §6.5 opt-out, which the owner rejected: a node that cannot trace does not index Ethereum at all, so no wallet can be in this state |
| `overdrawn` | The fold went negative — a proof of incompleteness (§5.2). Should never appear; if it does it is a bug or a missed cause |

`X` is a query parameter (`?confirmations=`) defaulting to the network's
`minimum_confirmations`, bounded the same way.

### 8.2 The history entry

Per change, the history row gains the one thing the owner asked for and does not
have: **the signed delta for the queried wallet**. The transaction id, block and
confirmations are already there (`sdk/wallets/src/history.rs:174-186`,
`:311-331`); the movements are already asset-scoped by `012`. What is missing is
that a caller must today reconstruct "how did this change my wallet" from
`from`/`to` themselves.

Add `delta: String` (signed, display units, of the query asset for the query
address) per transaction, and `cause: "transaction" | "block"`.

**Resulting balance per row: defer.** It requires the fold to be complete from
`B` to that row, making page *n* depend on pages `1..n-1`, and it is actively
misleading when `completeness != complete`. If it is wanted later, it belongs
behind an explicit `?running_balance=true` on ascending pages from the start,
computed store-side with a window function, never as a default.

### 8.3 Storage: exactly what changes

**Nothing already stored changes.** The fold reads the rows that exist today.
This deserves emphasis: `payments_movement` already carries
`(address, asset_chain, asset, amount, from_address, to_address, height)`
(`migrations/0001_init.sql:90-108`), duplicated per touched address
(`sdk/indexing/postgres/src/columns.rs:140-170`), which is precisely the shape an
aggregate needs. The redb history key already sorts by height
(`keys.rs:30-50`). The balance fold needs no migration at all.

What the **new causes** of §4 and §6 need:

| Backend | Change | Kind |
|---|---|---|
| Postgres | `0003_block_credit.sql`: new table `payments_block_credit (chain, network, address, height, credit_id, asset_chain, asset, amount)` + journal companion for the reorg cascade | **New migration.** `0001_init.sql` is frozen by checksum (`docs/INDEXING.md:158-163`), so this must be a new ordered file |
| Postgres | Index `payments_movement (chain, network, address, asset_chain, asset, height)` | **Additive**, `CREATE INDEX CONCURRENTLY`, no row touched |
| Postgres | `schema.rs` `EXPECTED_COLUMNS` / `EXPECTED_CONSTRAINTS` / `EXPECTED_INDEXES` (`:70-185`) | Constants only. `schema.rs` **validates**, it does not generate DDL; its `initializer_matches_validation` test (`:524-549`) parses the `.sql` and will fail until they agree |
| redb | New key tag `BLOCK_CREDIT: u8 = 7` beside the existing six (`keys.rs:7-13`), and `JournalRecord` (`record.rs:134-141`) gains the credit keys to delete on unwind | **No DDL, no data rewrite.** redb has exactly two physical tables, `data` and `meta` (`packages/storage/redb/src/backend.rs:23-24`); collections are key-tag prefixes. Existing tags are untouched and existing rows decode unchanged |
| Both | `BalanceStore` implementation + `BalanceContract` scenarios | New code |

On `AGENTS.md:352-356` — "Do not add … secondary address indexes … or raw-block
archives": the new Postgres index is a *physical* index on an existing table, not
a new address→data collection, and no raw block or trace is archived anywhere
(§6.4). The rule is respected as written and as meant. `payments_block_credit` is
a new fact table for a new kind of fact, which the same document's
preservation-first migration rules explicitly allow.

**Historical rows do not gain new causes retroactively.** Withdrawals, traced
internal transfers and Solana residuals appear only in blocks indexed after the
change. Making them appear for existing history requires a rescan — which is
`019` §5.3's `BlockStore::purge_network` and its offline
`payment-server --rescan <chain>/<network>`. Without that, every deployment
upgrading in place gets a balance that is correct only from the upgrade height
forward, and `completeness` must say so.

## 9. Migration

### 9.1 Dependencies on other documents

| Document | Relationship |
|---|---|
| `019` §6 (birthday refused below checkpoint) | **Prerequisite.** Today a past birthday silently indexes nothing (`019:535-542`), so a wallet would report a fold over zero rows as a balance. The endpoint cannot claim any completeness until this lands |
| `019` §5.3 (`purge_network` + offline rescan) | **Prerequisite for §8.3's retroactive causes**, and the tool the §7.5 backfill rule points operators at |
| `019` §2 (per-chain isolation) | **Enables §6.5.** A traceless Ethereum quarantines Ethereum only |
| `012` (history query by asset) | **Already met.** `HistoryQuery { address, asset, after, limit }` and the Postgres `EXISTS` predicate (`read.rs:26-39`) are shipped; without them a token fold would page through native transactions |

### 9.2 Commit order, every gate green after each

| Step | Commit | Effort | Gates |
|---|---|---|---|
| 1 | `feat(indexing): fold attributed movements into a balance` — `sdk/indexing/src/balance.rs`: `effect()`, `BalanceQuery`, `BalanceAt`, `Completeness`, `BigUint` fold with the overdraw branch. Pure, unit-tested, unwired | **S** | all |
| 2 | `feat(indexing): declare how a chain settles its network fee` — the §2.2 rule, so Bitcoin cannot double-count | **S** | all |
| 3 | `feat(indexing): add the BalanceStore port` + redb range fold + Postgres aggregate + `0003` index + `BalanceContract` in both backends | **M** | all |
| 4 | `refactor(bitcoin): derive balance from indexed movements` — the fold and the UTXO sum must agree; assert it in the acceptance test. Bitcoin first because it is complete by construction | **S** | all |
| 5 | `feat(api): report balance as of a block at a confirmation depth` — §8.1 fields, OpenAPI contract, `docs/API.md` | **S** | all |
| 6 | `feat(ethereum): index consensus-layer withdrawals` — `Head` keeps `withdrawals`, Gwei→Wei, `BlockCredit`, `0003_block_credit.sql`, redb tag 7. **No new RPC call** | **M** | all |
| 7 | `refactor(solana): make pre/post balances the authority for SOL` — residual movement instead of terminal rejection; the guard becomes an assertion | **M** | all |
| 8 | `refactor(ethereum): balance from the index, not the node` — the two account chains stop calling `eth_getBalance` / `getBalance` for balance | **S** | all |
| 9 | `feat(ethereum): attest a derived balance against a pinned block` — §7.3 witness, opt-in, never the returned amount | **S** | all |
| 10 | ~~`feat(ethereum): extract contract-initiated value from block traces`~~ — **done**: probe, `RawBlock` widening, `native/block/trace.rs`, residual movements. No config field and no §6.5 fallback: see §6.5 | **L** | all |
| 11 | `feat(ethereum): backfill ERC-20 history from transfer logs` — `eth_getLogs`, the §7.5 birthday rule. Depends on `019` §5.3 | **M** | all |

Steps 1–5 are independently shippable and leave Ethereum and Solana exactly as
they are; the balance endpoint does not change its source until step 8. Step 4
proves the mechanism against a chain whose answer is already known to be right,
which is the point of doing Bitcoin first.

**First commit: step 1.** It is pure, it is testable without a node or a store,
it settles the signed-delta and fee questions in code rather than in prose, and
every later step depends on it.

### 9.3 Effort

| Area | Effort |
|---|---|
| `indexing` fold + port + contract scenarios | M |
| redb + Postgres implementations, one migration | M |
| Bitcoin switch-over | S |
| Ethereum withdrawals | M |
| Solana pre/post authority | M |
| API + OpenAPI + docs | S |
| Ethereum traces (probe, config, evidence, interpreter, fallback) | **L** |
| ERC-20 `getLogs` backfill + birthday rule | M |

## 10. What this document does not change

- The `ValueMovement` vocabulary (`observation.rs:18-76`) and `MovementKind`
  (`sdk/base/src/movement.rs:3-14`). No variant is added; a `BlockCredit` is a
  different kind of record, not a sixth movement.
- `Decimal`'s refusal to do arithmetic (`packages/decimal/src/lib.rs:19-22`). The
  fold works in `BigUint` through the existing atomic-unit conversions.
- The non-negativity invariant on stored amounts at all three layers
  (`decimal/src/lib.rs:74-79`, `observation.rs:355`,
  `redb/src/amount_record.rs:20-22`). Signs stay derived.
- How confirmations are computed (`sdk/indexing/src/indexer.rs:99-110`) or the
  fact that they are never stored (`observation.rs:179-180`).
- Cursor semantics and the checkpoint-conflict rule
  (`indexer.rs:44-52`, `postgres/src/read.rs:83-96`).
- Bitcoin's and Solana's evidence structs. Only Ethereum's `RawBlock` widens
  (§6.4); Solana's `rewards` flag changes bytes, not shape.
- `019`'s isolation design, which this document depends on and does not
  re-litigate.
