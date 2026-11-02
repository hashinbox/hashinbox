# 066: Broadcast

Status: implemented.

A broadcast transaction is spent money. From the moment a node may have
accepted it, the funds it moves count as gone, whether it is mined in the next
block, waits in a mempool, or never lands. Until it is mined or expires,
nothing else may spend them again: no second trigger run, no manual send, no
other sender sharing the wallet.

## Use cases

1. **No double spend.** A wallet with 10 ETH sends 4. While that transaction
   is unmined, the wallet has 6 to spend, for every caller.
2. **No coin reuse.** A Bitcoin send spends coins A and B. While it is
   unmined, the next send from that wallet does not pick A or B.
3. **Funds come back.** A transaction that can never land stops holding its
   funds after its expiry, and its coins are spendable again.
4. **A refusal costs nothing.** A transaction the node refuses is forgotten at
   once; nothing stays held.
5. **One door.** Every send of the server (API, triggers) goes through the
   same queued broadcaster.

## Principles

- **Recorded is spent.** A transaction is recorded as pending before it is
  broadcast; its outgoing legs count against the balance from then on.
- **Derived, never marked.** Nothing marks a coin spent or a balance reduced.
  Both are read from legs: a pending transaction's legs count while its row
  exists; deleting the row (refused, expired, short of funds) releases
  everything at once.
- **Every pending transaction expires**, at a time its chain's builder sets.
- **The caller guards; the queue orders.** The SDK builds, signs and sends;
  it tracks nothing in flight. The server's queue topics (067) make every
  check race-free: only the broadcast job checks funds, and nothing of its
  network or sender runs beside it.
- **Built once.** A broadcast job signs once; its transactions are recorded
  with its state in one write, so a rerun only rebroadcasts.

## Design

### `rpc` (was `json-rpc`)

The package is `rpc`: the node transport, of which JSON-RPC is the one
encoding. (In chain crates, which have their own `rpc` module, it is
`::rpc`.)

```rust
pub trait Client: Send + Sync {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<CallResult, Error>>;
    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<CallResult>, Error>>;
    /// Sends `request` (a signed transaction) once to every node a layer
    /// reaches. Success or failure only; a node's error object is
    /// `CallError::Remote`.
    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), CallError>>;
}
```

`Cluster`, `Failover`, `Retry` and `Swap` keep their broadcast rules.

### `base`

```rust
pub struct Transaction {
    pub id: Id,
    pub state: State,
    pub order: Option<u64>,
    pub legs: Vec<Leg>,
    pub bytes: Vec<u8>,
    /// Unix ms after which a pending transaction stops holding its funds;
    /// `None` once mined.
    pub expires: Option<u64>,
}

pub trait Broadcaster: Send + Sync {
    fn broadcast<'a>(&'a self, transaction: &'a Transaction) -> Future<'a, Result<(), Error>>;
}
```

- `Enveloped` and `TransactionStore` are gone (with `Storage::transactions`).
- `Transactions::expiring(ttl)` sets `expires` on what a builder signs.
- The caller's duties are documented on `Broadcaster`: record before
  broadcasting; build one at a time per sender; forget on `Rejected` or
  `Expired`, keep and rebroadcast on a transient error; forget past
  `expires`.

### Chains

Each `Config` has `pending_ttl_ms`; the builder sets `expires = now + ttl`.

| Chain | Default `pending_ttl_ms` |
| --- | --- |
| Bitcoin | 259,200,000 (3 days) |
| Ethereum | 3,600,000 (1 hour) |
| Solana | 120,000 (past a blockhash's life) |

Solana's `lastValidBlockHeight` is a block height, while the server keys
blocks by slot, so a height expiry could not be compared; a time is used
everywhere. A Solana transaction past its blockhash fails to broadcast as
`Expired` and is forgotten then anyway.

### Server

#### Schema

`"transaction"` gains `sent_at` and `expires_at` (Unix ms), both `NULL` once
mined. A fresh database is needed.

#### Models

- `transaction::db::insert_pending(tx, now)`: stores `sent_at`, `expires_at`.
- `transaction::db::delete_by_id(id)`: deletes a pending row and its legs.
- `transaction::db::expire(network, now)`: deletes pending rows past
  `expires_at`, with their legs.
- `transaction::db::{exists, find_pending}`; `upsert_mined` clears the
  expiry columns.
- `leg::db` `UNSPENT`: a coin is spent when a `Spent` leg consumes it in a
  live block **or** in a pending transaction of the same network.
- `leg::db::find_pending_out(address, asset, except)`: outgoing legs in
  flight; `leg::db::delete_for(transaction)`: a pending transaction's legs.

#### Jobs and topics

| Job | Topic | Does |
| --- | --- | --- |
| `triggers` | `chain:network` | decide; an action starts a run and its broadcast jobs (068) |
| `broadcast` | `chain:network.sender` | sign, check and record, broadcast |

A triggers job locks the whole network, so it runs only once every earlier
broadcast of that network has ended, and the broadcasts it starts run after
it. When triggers decide, every transaction sent before is pending or gone,
never in flight.

#### The broadcast job (`workers/broadcast/`)

Input: an intent (sender, asset, transfers). State: the recorded txids and
how many the nodes accepted.

1. **Build and sign** (first run only): the chain's builder excludes coins
   spent by pending transactions.
2. **Check and record, in one write:** for every (address, asset) each
   transaction takes from, live mined legs plus pending outgoing legs plus
   its own must not go below zero; then it is recorded as pending, and the
   job's state saved with the txids. Short: nothing recorded, the job fails
   `insufficient-funds`.
3. **Broadcast** each recorded transaction not yet accepted, in order,
   through the network's own broadcaster (`Payments::broadcast`):
   - accepted: the count is saved; it holds its funds until mined or expired;
   - transient: retry, the same recorded bytes;
   - anything else: it and the rest are forgotten; the job fails.

A rerun never builds again; a recorded transaction forgotten meanwhile
(expired) fails it as `outcome-unknown`.

#### Expiry

Each network's `expire` job runs `expire(network, now)` every minute (068).

## Tests

- The broadcast job records, then broadcasts each; a retry resends the
  recorded bytes and never builds again; a refusal forgets what is not
  sent; a forgotten transaction on rerun is `outcome-unknown`; a sender
  short of funds sends nothing.
- A pending spend hides its coin from `find_unspent` until it expires.
- Topics: a triggers job waits for every earlier job of its network; `*.w1` locks
  `w1` on every network.
