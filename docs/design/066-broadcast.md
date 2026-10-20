# 066: Broadcast

Status: draft.

A broadcast transaction is spent money. From the moment a node accepts it,
the funds it moves count as gone, whether it is mined in the next block, waits
in a mempool, or never lands. Until it is mined or expires, nothing else may
spend them again: no second trigger run, no manual send, no other sender
sharing the wallet.

Today nothing enforces that. Pending transactions are never stored, balances
and Bitcoin coin selection read only mined history, and a trigger without
`idle` sends again while its first send waits in the mempool (send 4 ETH,
then 4 more, from a wallet holding 10).

## Use cases

1. **No double spend.** A wallet with 10 ETH sends 4. While that transaction
   is unmined, the wallet has 6 to spend, for every caller.
2. **No coin reuse.** A Bitcoin send spends coins A and B. While it is
   unmined, the next send from that wallet does not pick A or B.
3. **Funds come back.** A transaction that can never land (Solana blockhash
   expired, dropped from every mempool, replaced) stops holding its funds
   after its expiry, and its coins are spendable again.
4. **A refusal costs nothing.** A transaction the node refuses is forgotten at
   once; nothing stays held.
5. **One door.** Every send of the server (API, triggers) goes through the
   same queued broadcaster, so the same check applies to all of them.

## Principles

- **Accepted is spent.** A transaction is recorded as pending before it is
  broadcast; its outgoing legs count against the balance from then on.
- **Check, record, send, one at a time.** Whether the funds suffice, the
  record, and the broadcast happen in one job of the network's queue lane, so
  two senders never both see the same funds as free.
- **Derived, never marked.** Nothing marks a coin spent or a balance reduced.
  Both are read from legs: a pending transaction's legs count while its row
  exists; deleting the row (refused, expired) releases everything at once.
- **Every pending transaction expires.** Each one carries when it stops
  holding funds: a chain height where the protocol says so (Solana), a time
  otherwise.
- **The caller guards.** The SDK only builds, sends and records a
  transaction; it knows nothing of balances or policy. Deciding whether a
  transaction may be sent is the caller's. The server does it in its
  broadcast queue, which wraps the network's `Broadcaster`.
- **Built once.** A send's transaction is built and signed once, inside the
  network's lane, and saved with the job; a rerun only rebroadcasts it, so a
  crash never makes a second transaction (new coins, new nonce).

## Design

### `rpc` (was `json-rpc`)

The package is renamed `rpc`: it is the node transport, of which JSON-RPC is
the one encoding.

`Client::broadcast` returns no result:

```rust
pub trait Client: Send + Sync {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<CallResult, Error>>;
    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<CallResult>, Error>>;
    /// Sends `request` (a signed transaction) once to every node a layer
    /// reaches. Success or failure only: the transaction's id is known before
    /// it is sent. A node's error object is an `Error` carrying its code and
    /// message.
    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), Error>>;
}
```

`Cluster`, `Failover`, `Retry` and `Swap` keep their broadcast rules
(fan out once; never repeat a `Request::once`; transport failure wins over an
error object, because that node may have accepted it).

### `base`

#### The transaction carries its expiry

```rust
pub struct Transaction {
    pub id: Id,
    pub state: State,
    pub order: Option<u64>,
    pub legs: Vec<Leg>,
    pub bytes: Vec<u8>,
    /// When a pending transaction stops holding its funds; `None` once mined.
    pub expires: Option<Expiry>,
}

/// When a pending transaction can no longer land, or is given up on.
pub enum Expiry {
    /// Unix ms: chains without a protocol expiry (Bitcoin, Ethereum).
    At(u64),
    /// The chain's block height after which it cannot be included (Solana's
    /// `lastValidBlockHeight`).
    Height(u64),
}
```

Each chain's builder sets it when it signs (see Chains).

#### The broadcaster takes the transaction

```rust
pub trait Broadcaster: Send + Sync {
    /// Sends `transaction` to the network.
    ///
    /// # Errors
    ///
    /// `Rejected` when a node refused it (it will not land), `Expired` when it
    /// can no longer land, `Unavailable` when no node answered (it may have
    /// landed).
    fn broadcast<'a>(&'a self, transaction: &'a Transaction) -> Future<'a, Result<(), Error>>;
}
```

A chain's broadcaster sends `transaction.bytes`. The doc no longer names
error kinds that do not exist.


#### What a caller must do

The SDK builds, signs and broadcasts; it does not track what is in flight.
A caller that sends money is responsible for:

1. **Recording before broadcasting.** A transaction is spent from the moment
   a node may have accepted it. Record it (its legs, its `expires`) before
   `broadcast`, and count its outgoing legs against the balance from then on.
2. **One builder at a time per network.** Coin selection and the Ethereum
   nonce see only what is recorded. Build, check and record a network's sends
   one after another (a lock or a queue lane), or two sends may pick the same
   coins or nonce.
3. **Reading `broadcast`'s outcome:**
   - `Ok(())`: a node accepted it; it holds its funds until mined or expired.
   - `Rejected`: a node refused it; it will not land. Forget the record.
   - `Expired`: it can no longer land (Solana blockhash). Forget the record.
   - `Unavailable`: no node answered; it may have landed. Keep the record and
     rebroadcast the same bytes; never build it again.
4. **Expiring.** A pending transaction past `expires` (its time, or the
   chain's height passing it) can no longer be relied on to land; forget it
   so its funds and coins are free again. A transaction mined later is found
   by the indexer like any other.
5. **Never building twice.** Keep a built transaction's bytes until its
   outcome is known; a retry rebroadcasts them. Building again picks new
   coins or a new nonce, and both could land.

These are written into the docs of `Broadcaster` and `Transaction::expires`.

### Chains

| Chain | Broadcast | Expiry set by the builder |
| --- | --- | --- |
| Bitcoin | `sendrawtransaction` of `transaction.bytes` | `At(now + pending_ttl)`, default 3 days |
| Ethereum | `eth_sendRawTransaction` | `At(now + pending_ttl)`, default 1 hour |
| Solana | `sendTransaction` | `Height(lastValidBlockHeight)` of the blockhash it signed with |

- `pending_ttl_ms` is a field of each chain's `Config`, with those defaults;
  Solana needs none.
- Solana's `rpc.blockhash()` returns the blockhash with its
  `lastValidBlockHeight` (`getLatestBlockhash` already answers both), and the
  builder keeps it.
- A chain's spend legs already name what they take: Bitcoin `Spent` legs name
  the coin (`coin_txid`, `coin_index`); account chains name the address and
  amount; fees are `Fee` legs.

Later, exact expiry where the chain allows it: a pending Ethereum transaction
is dead once its sender's mined nonce passes its own; a pending Bitcoin
transaction is dead once a mined transaction spends one of its coins. Both
are found by the indexer and forgotten by the caller.

### Server

#### Schema

`"transaction"` gains, both `NULL` once mined:

```sql
sent_at        INTEGER, -- Unix ms the pending transaction was recorded.
expires_at     INTEGER, -- Unix ms after which it is given up (Expiry::At).
expires_height INTEGER, -- Block height after which it cannot land (Expiry::Height).
CHECK (block IS NULL OR (sent_at IS NULL AND expires_at IS NULL AND expires_height IS NULL))
```

A fresh database is needed (no migrations).

#### Models

These are the server's, not the SDK's: the broadcast job calls them on its
own connection, inside its one write.

- `transaction::db::insert_pending`: stores the expiry columns.
- `transaction::db::delete_by_id(id)`: deletes the row and its legs.
- `transaction::db::expire(network, now, height)`: deletes the pending rows
  of `network` with `expires_at <= now` or `expires_height < height`, and
  their legs.
- `transaction::db::upsert_mined`: also clears the expiry columns.
- `leg::db` `UNSPENT`: a coin is spent when a `Spent` leg consumes it in a
  live block **or** in a pending transaction of the same network.
- `balance::db::spendable(address, asset)`: live mined legs plus pending
  outgoing legs; what the `broadcast` job checks. `Balance` keeps `confirmed`
  and `pending`; the API shows `spendable` too.

#### Expiry

On each block notification of a network (`Notifying`), before triggers
decide: `expire(network, now, height of the last kept block)`. Released
funds and coins are visible to the round that follows.

#### Layout

```
workers/
  broadcast/
    mod.rs   the queued sender: `send(intent)` stores a job on the network's
             lane and returns once it is stored.
    job.rs   the broadcast job: build, check, record, broadcast (below).
  trigger/
    mod.rs   on block notifications (and at start, and on a slow fallback),
             enqueues one trigger round per network.
    job.rs   the round: settle runs, find touched and held triggers, decide
             (filter, check, plan); each plan becomes sends to `broadcast`.
```

`apps/server/src/trigger/` goes: its procedures move to `workers/trigger/`,
and the trigger's semantics (filter, check, amount, send) become methods of
the trigger model types. Each queue has one lane per network; the lane is the
lock.

#### The broadcast job

The server's queue decorates the network's `Broadcaster`: callers hand it an
**intent** (sender, asset, transfers), never a signed transaction, and the job
builds the transaction inside the network's lane, so coin selection and nonce
see every earlier pending send.

```rust
/// What a send asks for: one asset from one sender, its transfers in order.
pub(crate) struct Intent { chain, network, address, asset, transfers: Vec<Payment> }

/// The queued sender of a network.
pub(crate) struct Sender { queue: Arc<dyn tasks::Store> }

impl Sender {
    /// Stores a `broadcast` job holding `intent` on its network's lane.
    pub(crate) async fn send(&self, intent: Intent) -> anyhow::Result<tasks::Id>;
}
```

The job carries its state in its payload:

```
intent --build--> built --broadcast--> done
                    |
                    +-- refused / expired --> failed (pending record deleted)
```

1. **Input `intent`.**
2. **Nothing built yet:**
   - build and sign the transaction (the chain's builder; Bitcoin coins and
     the Ethereum nonce already exclude what earlier pending sends took);
   - **check** it against the spendable balance: for every (address, asset)
     it takes from, live mined legs minus pending outgoing legs cover its
     outgoing legs (spent, burned, fee). If not, the job fails
     `insufficient-funds`; nothing is recorded;
   - in **one write**: record it as pending (`transaction::db::insert_pending`,
     with its expiry) and save `built` (txid, bytes) into the job's state. The
     queue and the transactions share one SQLite file, so the two never
     disagree. A crash before the write leaves nothing: the rerun builds again
     and nothing was sent.
3. **Built** (first run, or any rerun): broadcast those exact bytes through
   the network's `Broadcaster` (the chain's rpc):
   - accepted: done; the record holds its funds until mined or expired;
   - `Rejected` or `Expired`: `delete_by_id`; the job fails;
   - `Unavailable`: the job retries, rebroadcasting the same bytes; if it
     never lands, its expiry releases it.

A transaction is never built twice for one job: a rerun only rebroadcasts.

Every sender goes through it: `POST /v1/wallets/{id}/transactions` and
trigger rounds hand intents to `Sender`. The task queue's own refactor
(results, idempotency keys, cancel) is a separate design.

#### Triggers

- Checks and amounts read the spendable balance, so a trigger never plans
  funds already in flight, whoever sent them.
- A run settles when none of its transactions is pending any more (mined or
  expired), read from `"transaction"` instead of job payloads.
- `Idle` stays as a gate a trigger may use, but is no longer what prevents a
  double send.

## Plan

1. `rpc`: rename `json-rpc`; `Client::broadcast -> Result<(), Error>`.
2. `base`: `Expiry`, `Transaction::expires`, `Broadcaster(&Transaction)`;
   the caller's duties documented on `Broadcaster` and
   `Transaction::expires`; `TransactionStore` removed (with its slot in
   `base::Storage` and the server's implementation).
3. Chains: broadcast `&Transaction`; builders set `expires`; Solana keeps
   `lastValidBlockHeight`; `pending_ttl_ms` in each `Config`.
4. Server: schema columns; models; `UNSPENT`; spendable balance; expiry on
   block notifications.
5. Server `workers/broadcast/`: `Sender` and the `broadcast` job (build,
   check, record and save in one write, broadcast); API sends through it.
6. Server `workers/trigger/`: the round as a queued job per network; trigger
   semantics onto the model types; `apps/server/src/trigger/` removed;
   triggers read spendable and settle on pending rows.

## Tests

- Queue: two broadcasts of 4 from a wallet of 6 (ETH): the first is sent,
  the second job fails `insufficient-funds`, nothing recorded for it.
- Queue: a refused broadcast leaves no pending row; `Unavailable` keeps it
  and retries.
- Rerun: a job killed after its write rebroadcasts the saved bytes and builds
  nothing; one killed before it builds again, and nothing was recorded.
- Bitcoin: after a pending send of coins A and B, `find_unspent` returns
  neither; after `expire`, both.
- Expiry: `At` past `now` and `Height` below the tip are deleted; others kept.
- Mined: `upsert_mined` of a pending transaction clears its expiry and keeps
  it as history.
- Concurrency: two broadcasts on one network, queued together, are checked
  one after the other (the second sees the first's pending legs).
- Triggers: a run settles when its transactions are mined, and when they
  expire.
