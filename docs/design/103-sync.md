# 103: Keeping a third party's backend in sync: ask, poll, stream

Status: proposed (doc only). Builds on 097 (`wallet_event`, not built yet) and pairs with 104
(hashpay: `wallet_group`, payment intents). Open questions are at the end.

## Problem

- A third party's backend credits its users and marks orders paid. It must see every deposit,
  confirmation and reorg, and every intent paid or expired, exactly as the server saw them.
- It must not miss one when it was down, and must not count one twice.
- Push (webhooks) as the sync mechanism is out: the receiver's uptime becomes our outbox, its
  bugs our retries, and it still has to reconcile. **The third party asks** instead.

## Rules

- **Ask any time:** `GET /v1/wallet_groups/{id}` (104), `GET /v1/wallets/{id}`: the state is the
  truth, never a message about it.
- **One feed:** 097's `wallet_event`, read by list (`GET /v1/events`) and stream (SSE). Same
  events, same ids, same JSON, same cursor.
- **Poll every ~30 s** from the last cursor: the safety net, always on. **SSE is the fast path**:
  it delivers sooner, never instead.
- **Cursor = the last record's `(at, id)`**, one opaque string. **At-least-once**; the event `id`
  is the idempotency key.
- **Every SSE route closes after ~15 s;** the client re-opens from its cursor.
- **Webhooks exist only as a trigger action** (`webhook`, beside `send`): a notification a
  trigger fires, delivered by a job, never the sync mechanism.

## Changes to 097 (made before 097 is built)

104 needs group context in the feed; the sync needs the cursor. 097 is built with these, no
separate migration (097 itself carries the cursor parts):

| Change | Why |
|---|---|
| `wallet_group_id TEXT REFERENCES wallet_group (id)`, NULL for a wallet in no group | the `?group=` filter, a group's events in order, the hashpay stream |
| kinds `paid`, `expired`: group events, `wallet_id` and `wallet_index_id` NULL, `subject` = the group id, `data.network` the network it is told on (below) | an intent's outcome lives in the same feed and cursor space as deposits |
| `data.received_usd`: Σ `amount_usd` of the event's received, minted and reward legs; NULL while unpriced | "deposited in USD" without netting a fee or a sweep |
| `at` monotone, cursor `(at, id)`, envelope `cursor` (097) | the timestamp cursor, below |

```sql
CREATE TABLE wallet_event (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    at              INTEGER NOT NULL, -- Unix ms, max(now, newest at): never goes back (097)
    wallet_id       TEXT,             -- NULL: a group event
    wallet_index_id TEXT    REFERENCES wallet_index (id) ON DELETE CASCADE,
    -- The wallet's group when it has one (a wallet joins a group when it is
    -- created and never leaves, so this never goes stale); a group event's own.
    wallet_group_id TEXT    REFERENCES wallet_group (id),
    kind            TEXT    NOT NULL,
    subject         TEXT    NOT NULL,
    data            TEXT    NOT NULL CHECK (json_valid(data)),
    CHECK ((wallet_index_id IS NULL) = (kind IN ('paid', 'expired'))),
    CHECK ((wallet_id IS NULL) = (wallet_index_id IS NULL)),
    CHECK (wallet_index_id IS NULL OR wallet_index_id LIKE wallet_id || '.%'),
    CHECK (wallet_group_id IS NOT NULL OR kind NOT IN ('paid', 'expired')),
    CHECK (kind IN ('pending', 'mined', 'confirmed', 'reorged', 'dropped', 'adjusted',
                    'paid', 'expired'))
);
-- 097's three indexes, plus a group's events in order:
CREATE INDEX wallet_event_group ON wallet_event (wallet_group_id, at, id)
    WHERE wallet_group_id IS NOT NULL;
```

- The envelope gains `wallet_group_id` (always present, null when none). `wallet_id` and
  `wallet_index_id` are null on a group event.
- A group event's `data.network`: for `paid`, the network whose `confirmed` settled it; for
  `expired`, the group's first member's (by wallet id). Only triggers read it (one network
  tells it once).
- **Visibility, one rule** (`WalletEvent::visible_to(scopes)`), used by lists, streams and the
  trigger's events term: an event is visible when the scopes cover
  `wallets[{wallet_id}].events.$read`, or it has a group and they cover
  `wallet_groups[{group}].$read`, or they cover `wallets.events.$read` (everything).

## The cursor: the last record's timestamp, with a tie-breaker

`cursor = "{at}-{id}"`, e.g. `"1795600000123-1042"`: the last record's `at` (Unix ms) and its
`id`. Clients store it and send it back; they never build or compare one.

- **Order:** `(at, id)`, `at` first. A page is
  `WHERE at > :at OR (at = :at AND id > :id) ORDER BY at, id`, on `(…, at, id)` indexes.
- **Why not `at` alone:** one write tells a whole block, so its events share one `at`; a page
  cut inside them either skips the rest (`at >`) or repeats them forever when more than `limit`
  share it (`at >=`). The `id` breaks the tie.
- **Why `(at, id)` misses nothing:** the server has one writer (`BEGIN IMMEDIATE`). An event's
  `id` and `at` are both taken **inside** its write, and `at = max(now, newest at)` (one seek on
  `wallet_event_at`), so a clock stepping back never lowers it. Writes commit one after another,
  so a later commit has a larger `id` and an `at` no smaller: `(at, id)` order is commit order.
  A reader that saw everything up to cursor C can never later find a committed event before C.
- **A plain timestamp taken outside the write would not be safe:** a write that took its `at`
  before a reader's snapshot but committed after it would land behind the reader's cursor.
- **Same order as `id`:** with `at` monotone, `(at, id)` sorts exactly as `id`. The `at` part is
  what the user reads ("synced up to 14:02:31"), what `since=<Unix ms>` maps to
  (`(at, id) >= (since, 0)`), and what retention compares (below).
- **Expired:** the prune job keeps `wallet_events_pruned_before` (its cutoff, Unix ms). A cursor
  whose `at` is below it gets `410 cursor-expired`: read the state (`GET` balances and groups),
  continue from now.

**Guarantee:** a reader that resumes from the last cursor it processed misses no event
(unless 410). **At-least-once:** after a reconnect or a crash before the cursor was saved, the
same event comes again; **dedupe by event `id`**. Order is total and the same on every path.

## Polling: `GET /v1/events`

097's list, two filters added:

| Query | What |
|---|---|
| `cursor`, `limit` | `IndexParams`; the cursor above, oldest first |
| `since` | Unix ms, ignored with `cursor` (097) |
| `group` | only that group's events: its members' and its `paid`/`expired` (`wallet_event_group`) |
| `kinds` | comma list, e.g. `confirmed,reorged,paid,expired` |

- Reply `{ events, next_cursor }`. `next_cursor` is the last listed event's cursor, null when
  none listed (keep yours); a page shorter than `limit` means caught up.
- Scope: `com.hashinbox.wallets.events.$read` for everything (unnarrowed, 096 lists); with
  `group`, `wallet_groups[{group}].$read` is enough. Model fns `wallet_event::db::find_page` and
  `find_by_group_page`.

## The 15 s rule: every SSE stream is a window

Every SSE route, everywhere: the server ends the stream after about 15 s, the client re-opens it
from its cursor. Clients reconnect constantly, by design.

| Piece | What |
|---|---|
| `api/sse.rs`: `SSE_WINDOW = 15 s` | the one constant; each stream closes at 15 s ± 2 s (jitter: no reconnect stampede after a restart), or at the token's `exp` if sooner |
| `windowed(stream, cursor)` | wraps every route's event stream: writes `retry: 500` first (`EventSource` re-opens in 0.5 s, with `Last-Event-ID`), then the events, then the closing event, then ends the body |
| closing event | `event: reconnect`, `data: {"cursor": "…" \| null}`: where to re-open; null for a stream without one. Its `id:` is the same cursor |
| re-open | `Last-Event-ID: <cursor>` (header) or `?cursor=`; the first wins |
| keep-alive | none: a window ends before any proxy's idle timeout |
| `x-events` | every stream lists `reconnect` → `StreamReconnect { cursor: string \| null }` |

| Route | Cursor | On re-open |
|---|---|---|
| `GET /v1/events/stream`, `/v1/wallets/{id}/events/stream` (097) | the last sent event's, or the start position when none was sent | replay from the DB after it, then live |
| `GET /v1/wallet_groups/{id}/deposits/stream` (104) | null: each message is a whole snapshot | a fresh snapshot |
| `GET /v1/logs/stream` | null: a tail, not a feed | live from now; the gap (≈ 0.5 s) may drop entries, as `missed` already does. A reader that needs every entry lists `GET /v1/logs?since=` |
| `GET /v1/networks/{chain}/{network}/tip/search` | `"{low}-{high}"`: the last probe's positions | the search resumes between them; `found` or `error` still ends it for good |

**Why:**

- **No half-dead connections.** Proxies, NATs and load balancers drop idle TCP silently; a
  client then waits forever on a stream that will never speak. A stream that must end within
  ~17 s lets every client use a 20 s read timeout: silence past it is a dead connection.
- **Credentials re-checked every 15 s.** Each re-open is a new request: session, scopes and
  key are checked again, so a revoked key, a logged-out session, an expired token or a narrowed
  user stops streaming within 15 s. Today a stream runs on the scopes checked at open.
- **Load balancing and deploys.** No connection pins to one instance for hours; a shutdown
  drains in one window.
- **Simpler servers.** No keep-alives, no lag bookkeeping across hours, a bounded lifetime per
  connection; the reconnect path runs constantly, so it works when it matters.
- **Cost:** 4 requests a minute per subscriber, each one indexed seek from its cursor.

Clients (101's hand-written `stream()`, every language): re-open on `reconnect` and on a clean
end, with the last cursor; read timeout 20 s; on an error, the backoff below. A stream ends for
the caller only on its `break`, cancel, or a final event (`found`, `error`).

## Client libraries: `subscribe` with `ack()`

Hand-written in each client (101), beside `stream()`: a callback per event plus `ack()`, which
moves the cursor. The caller stores the cursor; **the library never persists anything.**

```ts
const sub = client.events.subscribe(
  { cursor: await store.load(), group, kinds: ["confirmed", "reorged", "paid", "expired"] },
  async (event, ack) => {
    await handle(event);              // idempotent by event.id
    await store.save(event.cursor);   // the caller's own storage
    ack();                            // the cursor moves; the next event comes
  },
  { onError: (error) => log.warn(error) },
);
// …
await sub.close();                    // or: await sub.done (rejects on a fatal error)
```

```go
sub, err := hashinbox.Subscribe(ctx, client, hashinbox.SubscribeOptions{
    Cursor: store.Load(), Group: group, Kinds: []string{"confirmed", "reorged", "paid", "expired"},
    OnError: func(err error) { log.Println(err) },
}, func(ctx context.Context, event hashinbox.WalletEvent, ack func()) error {
    if err := handle(ctx, event); err != nil { return err }       // not acked: redelivered
    if err := store.Save(ctx, event.Cursor); err != nil { return err }
    ack()
    return nil
})
if err != nil { return err }       // bad options only; the loop runs in its own goroutine
defer sub.Close()                  // stops, waits for the callback in flight
err = sub.Wait()                   // nil after Close or ctx done; ErrCursorExpired, *APIError (403)
```

**The loop** (the same in every language):

1. `acked` = the given cursor (absent: from now; the first `reconnect` or page gives one).
2. **SSE:** open `GET /v1/events/stream` (or `/v1/wallets/{id}/events/stream` with `wallet`)
   from `acked`; on `reconnect` or a clean end, re-open from `acked`.
3. **Poll:** every 30 s, `GET /v1/events?cursor=acked&limit=500` (same filters), paging while
   full. Runs whether SSE is healthy or not.
4. Both feed one queue. An event whose `id` is at or below the last one handed out is skipped
   (ids grow with cursors), so SSE and poll never deliver one twice in a row.
5. **One callback at a time** per subscription, in cursor order. The next event is handed out
   only after the callback returns **and** called `ack()`.
6. `ack()` sets `acked = event.cursor`. It is cumulative (everything before is acked too) and
   counts only before the callback returns.
7. **Not acked** (returned without `ack()`, threw, or returned an error): the window is dropped,
   the library waits the backoff, and re-opens from `acked`: **the unacked event comes again**,
   then the ones after it.

| Rule | Choice |
|---|---|
| Ordering | total, by cursor; never reordered, never skipped past an unacked event |
| Concurrency | one callback at a time per subscription; for parallelism, several subscriptions (e.g. per group) |
| Backoff | network errors, 5xx, 429 (`Retry-After` wins), an unacked event: 1 s, 2 s, 4 s … 30 s, ± 20 % jitter; reset after a window or a poll succeeds |
| Fatal | `410 cursor-expired` (`CursorExpired`: read the state, subscribe from now), `403` (missing scope), `400`: the subscription ends; `done` rejects / `Wait` returns it |
| 401 | the client's own re-signing (101); a second 401 is fatal |
| `close()` | aborts the stream and the poll, waits for the callback in flight (its `ack()` still counts), then resolves; idempotent |
| State | in memory only: `acked`, the last handed-out id. A restart resumes from what the caller stored |

Other languages, the same loop and rules:

| Language | Shape |
|---|---|
| Rust | pull: `let mut sub = client.subscribe(Subscribe::new().cursor(c).group(g));` then `while let Some(delivery) = sub.next().await? { handle(&delivery.event).await?; store.save(&delivery.event.cursor).await?; delivery.ack(); }`; a `Delivery` dropped unacked rewinds; `sub.close().await` |
| Python | `async with client.subscribe(cursor=c, group=g) as sub:` `async for event, ack in sub: await handle(event); await store.save(event.cursor); ack()`; leaving the block closes; an iteration without `ack()` rewinds |
| PHP | blocking: `$client->subscribe(new Subscribe(cursor: $c, group: $g), function (WalletEvent $event, callable $ack): void { …; $ack(); });` SSE windows, with the poll between windows when 30 s have passed. `$client->poll($cursor, $fn): string` pages once to caught up and returns the new cursor: for cron (WordPress) |

- TS works in browsers, Node, Deno, Bun (fetch streams); a browser subscriber uses a narrow
  token (`wallet_groups[g].$read`).
- Tests: a fake server per language: an unacked event comes again after the backoff; SSE and
  poll overlapping deliver once; `close()` waits for the callback; 410 is fatal; a
  `reconnect` re-opens from the acked cursor, not the last sent one.

## Webhooks: a trigger action

A trigger (065, 077) that fires `webhook` POSTs what it saw to a URL. It notifies; the receiver
still trusts only the state it reads (`GET`) or the feed.

### When it fires: the `events` check term

Triggers decide on balances at the confirmed block. "A deposit on a group" or "an intent paid"
is an event, so the server's `Check` gains one term:

```json
{"events": {"kinds": ["confirmed", "paid", "expired"]}}
```

- Holds when the kept indexes have events of those kinds after the trigger's **mark**: their
  own, and their groups' `paid`/`expired` whose `data.network` is the trigger's network (so a
  group spread over four networks tells it once).
- The mark is a cursor: the newest event the round's read could see. Each round moves it, in the
  round's one write; the next round reads after it (an index not kept that round is not told
  its events later). Same proof as the feed: nothing committed
  after the read sorts before it.
- Check only (`reads_this` false): a filter keeps indexes by balance, as today.
- **Package change** (`sdk/trigger`, small): `Store::events(indexes, after) -> Vec<Value>`,
  `Context::events()` (the kept indexes' events after the mark, read once per round through
  `CachedStore`), and `LastRound.mark: Option<String>`.

### The action

```json
{
  "id": "shop-paid-bnb", "chain": "bnb", "network": "mainnet",
  "selector": {"tag": "hashpay"},
  "check": {"events": {"kinds": ["confirmed", "paid", "expired"]}},
  "action": {"name": "webhook", "input": {"url": "https://shop.example/hooks/hashinbox",
                                          "secret": "whs/4mQ…"}}
}
```

| Input | Rule |
|---|---|
| `url` | absolute `https://`, or `http://` to a loopback host; no credentials in it. Private addresses allowed (self-hosted; the receiver is often on the LAN) |
| `secret` | `whs/` + base 58 of 32 random bytes and their CRC32 (as `his/`, 096); made by the client's `Webhook.secret()` or the dashboard. Stored in the trigger; read back as `whs/…` + its last 4 characters; a replace without `secret` keeps the stored one |

- `run`: one `webhook` job per run, its input the frozen body and the trigger id (never the
  secret); no job when nothing is kept.
- **Scope:** creating or replacing a trigger needs `com.hashinbox.triggers.$write`. The
  `wallets.$write` check moves from the route to `send` triggers only.
- Registered beside `send`: `Registry::new().add(Transfer).add(Webhook)`.

### The body: the run

```json
{
  "id": "shop-paid-bnb.1795600000123",
  "trigger": "shop-paid-bnb",
  "network": "bnb.mainnet",
  "at": 1795600000123,
  "confirmed": 21000059,
  "wallets": [{ "wallet_index_id": "bnb.mainnet.0xabc.0x55d3…", "address": "0xabc",
                "asset": "0x55d3…", "balance": { "…": "097's snapshot" } }],
  "events": [{ "id": 1061, "cursor": "1795600000101-1061", "kind": "confirmed", "…": "097 envelope" },
             { "id": 1062, "cursor": "1795600000101-1062", "kind": "paid", "…": "…" }]
}
```

- `id` is the trigger run's id: the delivery id. `wallets` is the kept set; `events` the new
  events of the kept indexes (every kind, oldest first), at most 500: past that, the mark stops
  at the 500th and the rest go next round.
- Frozen in the job's input when the round decides: every attempt sends the same bytes.

### Delivery: the job, its retries, the run log

Worker `webhook` on `packages/tasks`:

| Rule | Choice |
|---|---|
| Request | one `POST`, `Content-Type: application/json`, 10 s timeout, no redirects; job timeout 15 s |
| Success | any 2xx → `Done`. Else (3xx, 4xx, 5xx, timeout, TLS, connect) → `Retry("HTTP 500: <first 200 bytes>")` |
| Retries | the queue's own: `Retry { attempts: 20, backoff: 60 s }`, linear: 1, 2 … 19 min, about 3 h in all. Then failed |
| Gone | the trigger was deleted, or no longer calls `webhook`: `Fail` |
| Order | none promised: no topic, concurrency 8; each body is whole on its own |
| Log | the trigger's runs page (`trigger_runs_list`) is the delivery log: each run's job, its status, attempts and last reason. Logs `trigger.webhook.delivered`, `trigger.webhook.failed` (url host, status, attempt), never a secret |

- No `webhook` table, no outbox, no management routes, no `webhooks` scope: a webhook is a
  trigger; create, change, disable and delete it as one.
- A receiver that was down past the retries loses those deliveries, not the events: it polls.

### The request

```http
POST /hooks/hashinbox HTTP/1.1
Content-Type: application/json
User-Agent: hashinbox/v0.5.0
Hashinbox-Trigger: shop-paid-bnb
Hashinbox-Delivery-Id: shop-paid-bnb.1795600000123
Hashinbox-Attempt: 3
Hashinbox-Signature: t=1795600000,v1=5257a869e7ecebeda32affa62cdca3fa51cad7e77a0e56ff536d0ce8e108d8bd

{"id":"shop-paid-bnb.1795600000123","trigger":"shop-paid-bnb",…}
```

### Signing: Stripe-style HMAC

- `v1 = hex(HMAC-SHA256(key = the whs/… text, message = t + "." + body))`, `t` = Unix seconds of
  this attempt.
- **Verify:** parse `t` and every `v1`; refuse `|now − t| > 300 s` (replay); recompute over the
  **raw** body bytes (never re-serialized JSON); compare in constant time; any `v1` matching
  passes. Then decode the body; dedupe by its `id` (the run) and each event's `id`.
- Server side: HMAC-SHA256 from the `hmac` crate in `apps/server` (`sha2` is already there);
  `packages/crypto` stays untouched.

Hand-written in each client (101), returning the generated `TriggerWebhook` model:

| Language | Call |
|---|---|
| TS | `await Webhook.verify(rawBody: string \| Uint8Array, header: string, secret: string, { tolerance = 300, now? })` → `TriggerWebhook`; `Webhook.secret()`; WebCrypto |
| Go | `hashinbox.VerifyWebhook(body []byte, header, secret string, now time.Time) (TriggerWebhook, error)`; `errors.Is(err, ErrWebhookSignature \| ErrWebhookTimestamp)` |
| Rust | `hashinbox_client::webhook::verify(body: &[u8], header: &str, secret: &str, now: SystemTime) -> Result<TriggerWebhook, WebhookError>` |
| Python | `hashinbox.verify_webhook(body: bytes, header: str, secret: str, tolerance: int = 300) -> TriggerWebhook` |
| PHP | `Webhook::verify(string $body, string $header, string $secret, int $tolerance = 300): TriggerWebhook` |

```ts
// Express: the raw body is what was signed. The webhook is a nudge: verify, then read.
app.post("/hooks/hashinbox", express.raw({ type: "application/json" }), async (req, res) => {
  const run = await Webhook.verify(req.body, req.get("hashinbox-signature") ?? "", secret);
  res.sendStatus(200);                       // answer fast
  for (const event of run.events) if (event.kind === "paid") await verifyAndFulfil(event.subject);
});
```

- `_golden/webhook.json` (secret, body, `t`, header) from the server's test keeps every port
  byte-identical, as `_golden/session.json` does.

## Which path for what

| | Who keeps the cursor | Fits | Order |
|---|---|---|---|
| `GET` the group / wallet | none: the state | proof before fulfilling; after any doubt | n/a |
| `GET /v1/events` every 30 s | the client | every backend; the safety net | total, by cursor |
| events SSE, 15 s windows | the client | sooner delivery for a long-lived worker or a dashboard | total, by cursor |
| `subscribe` (both, with `ack()`) | the caller, via `ack()` | the default for a backend | total, by cursor |
| `GET /v1/wallet_groups/{id}/deposits/stream` (104) | none: whole snapshots | the browser checkout | latest wins |
| trigger `webhook` | the trigger's mark | a backend that wants a nudge (PHP, serverless) | none |

## For a hashpay group: what each kind means

| Kind | Means for a deposit address | Backend does |
|---|---|---|
| `mined` | in a block, not final; `received_usd` is its value | show "pending"; never credit |
| `confirmed` | final (≤ `finality_at`); once per `(wallet_index_id, subject)` (097's unique index) | balance mode: credit `received_usd` to the group's external id |
| `reorged` | a block took it back (only ever before `confirmed`, 088) | drop the "pending" |
| `paid` | a one-time intent's confirmed USD reached its amount (104) | `GET` the group, fulfil the order |
| `expired` | a one-time intent ended unpaid (104) | cancel or ask for the rest |
| `pending`, `dropped`, `adjusted` | the server's own sends, and settles (097) | sweeps and bookkeeping; a shop filters them out (`kinds`) |

## Code placement

| Piece | Where |
|---|---|
| `SSE_WINDOW`, `windowed`, `StreamReconnect` | `api/sse.rs`; every stream route wraps its events in it |
| cursor parse and format, `find_page` and `find_by_group_page` on `(at, id)` | `models/wallet_event.rs` |
| `events` check term | `models/trigger.rs` (`Check::Events`); `Store::events` in `usecase/trigger/store.rs` |
| `Webhook` action, its input checks, secret redaction | `usecase/workers/trigger/actions.rs` |
| the `webhook` job, signing (`sign(secret, t, body) -> header`) | `usecase/workers/webhook.rs` |
| `stream()` reconnect, `subscribe`, `Webhook.verify`, `Webhook.secret` per language | `apps/server/clients/<lang>/`, `_golden/webhook.json` |
| dashboard | the trigger form gains the `webhook` action and the `events` term; runs show deliveries |

## Migration

- None of its own: `wallet_event` ships with 097 as above; triggers keep their table (the action
  and the term are JSON in it; `last_round` gains `mark`).
- New setting: `wallet_events_pruned_before` (written by the prune job, private).

## Tests

- Cursor: a write of 1,200 events with one `at` pages through `limit` 500 without a skip or a
  repeat; a clock stepped back keeps `at` monotone; a writer that commits after a reader's
  snapshot is read after the reader's cursor; `since` maps; below `pruned_before` is 410.
- Windows: every stream ends at 15 s ± 2 s with `reconnect`; `Last-Event-ID` and `?cursor=`
  resume; a revoked key's stream gets 401 on the next re-open; the tip search resumes from
  `low-high`; the deposits stream sends a snapshot per window.
- `GET /v1/events?group=` lists the members' events and `paid`; `kinds` filters.
- Subscribe, per language: as listed above.
- Trigger: `events` holds once per new event, never twice across rounds; a group's `paid` fires
  only the trigger of its `data.network`; a webhook trigger without `wallets.$write` is created;
  `send` still needs it; the secret reads back redacted.
- Delivery against a loopback receiver: 2xx done; 500 retried by the queue; 20 failures fail;
  a slow receiver times out at 10 s; the runs page shows status and reason.
- Signature: the golden vector in every client; a stale `t`, a changed byte and a wrong secret
  are refused.

## Plan

| Phase | Scope | Effort |
|---|---|---|
| 0 | 097 built with the changes above (table, `(at, id)` cursor, writers, list, stream, hub) | 5–6 days |
| 1 | `api/sse.rs` and the 15 s windows on the four routes (tip search resume); `stream()` reconnect in the five clients | 2–3 days |
| 2 | `subscribe` + `ack()` in TS and Go, tests | 3–4 days |
| 3 | `subscribe` in Rust, Python, PHP (with `poll()`) | 3 days |
| 4 | `events` term (package change), `webhook` action and job, signing, redaction, TS and Go `verify`, dashboard form | 4–5 days |
| 5 | Rust, Python, PHP `verify` | 1 day |

## Risks

- **Reconnect load:** 4 requests a minute per subscriber; thousands of browser checkouts are
  thousands of short requests. The rate limits (104) count streams open, not re-opens.
- **A receiver that answers 2xx and then drops the run** loses that nudge: it polls anyway.
- **Readable secrets** (`whs/` in the trigger row, like `his/`): a database leak lets someone
  forge deliveries. Replace the secret after a leak.
- **Clock skew** on the receiver beyond 300 s refuses every delivery: keep NTP.
- **Webhook bodies carry wallet data** to a URL anyone with `triggers.$write` chose, and sit in
  job inputs (`jobs.$read`).

## Open questions

1. **Webhook scope:** `triggers.$write` alone (decided), or also `wallets.events.$read`, since a
   body ships wallet balances and events?
2. **Group events in triggers:** told on one network through `data.network` (proposed), or
   network-less triggers for group events?
3. **Retry window:** the queue's linear 20 × 60 s, about 3 h (proposed), or a longer one (more
   attempts)? Past it the receiver polls.
4. **Redelivery:** none in v1 (polling covers it), or a `POST /v1/jobs/{id}/retry`?
5. **http:// to private hosts:** allow plain http to private ranges (a backend on the LAN), or
   https and loopback only (proposed)?
6. **Secret rotation:** replace in one step (proposed), or keep a previous secret signing for
   24 h (two `v1=`)?
7. **`/v1/events` and group events:** is `wallets.events.$read` the right scope for `paid` and
   `expired` (proposed), or should they need `wallet_groups.$read`?
8. **Window length:** 15 s for every stream (decided); a per-route override for the logs
   dashboard if the gaps show?
