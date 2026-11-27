# 104: hashpay: payment intents, wallet groups and the pay SDK

Status: proposed (doc only). Needs 097 (`wallet_event`, with 103's changes) and 103 (sync: the
events feed, 15 s stream windows, `subscribe`).
Open questions are at the end.

## Problem

- A shop wants crypto checkout: the user picks a currency, scans a QR, sees "pending", then a
  checkmark. The shop's backend learns it was paid.
- Two modes, same machinery:
  - **one-time:** an order of a USD amount; paid when that much has arrived;
  - **balance:** a top-up address per customer; each deposit raises their balance.
- Users send the wrong asset or the wrong network (choose ETH, send BNB). Money that lands on an
  address the server does not watch is lost to the shop.
- A shop must not run any of this in the browser with a secret.

## Concepts

| Term | What it is |
|---|---|
| **Wallet group** (`wallet_group`) | One customer's (or order's) set of wallets: one per network, every asset of each indexed. It carries the intent: mode, USD amount, expiry, status |
| **Payment intent** | What the shop asks for: `POST /v1/wallet_groups`. The answer is the group plus an **intent token** |
| **Intent token** | A narrow JWT (096): `com.hashinbox.wallet_groups[{id}]:read`. The browser watches the group with it |
| **Deposit** | One incoming transaction on one index of the group: 097's events for `(wallet_index_id, subject)`, folded |
| **hashpay** | The brand: the headless TS core and the web components that render a checkout |

### Intent on the group, not a separate table

| Option | Verdict |
|---|---|
| **Intent fields on `wallet_group`** | **Chosen.** One-time makes a new group per intent, so the two are 1:1. Balance mode has no amount and no expiry; its "session" is the token, and `since` (the token's `iat`) is its baseline. One table, one id, one scope |
| `payment_intent` table referencing a reused group | Needed only for many one-time intents on one set of addresses. That saves indexes but mixes two orders' deposits on one address; not worth a second resource and id |

## Tables

```sql
-- A set of wallets watched together for one external id (design 104), and
-- the payment intent it was made for. mode 'one_time': amount_usd and
-- expires_at set, paid once its confirmed deposits reach amount_usd less
-- tolerance_bps. mode 'balance': neither; never paid or expired; one per
-- (external_id, testnet), reused. watch_until: its indexes are paused after
-- it (NULL: never). Times are Unix ms.
CREATE TABLE wallet_group (
    id            TEXT    PRIMARY KEY,                -- uuid v7
    external_id   TEXT    NOT NULL CHECK (length(external_id) BETWEEN 1 AND 128),
    mode          TEXT    NOT NULL CHECK (mode IN ('one_time', 'balance')),
    testnet       INTEGER NOT NULL CHECK (testnet IN (0, 1)),
    -- What the checkout offers: network asset ids, "{chain}.{network}.{asset}".
    accept        TEXT    NOT NULL CHECK (json_valid(accept)),
    amount_usd    TEXT,                               -- exact decimal > 0
    tolerance_bps INTEGER NOT NULL DEFAULT 50 CHECK (tolerance_bps BETWEEN 0 AND 1000),
    status        TEXT    NOT NULL CHECK (status IN ('open', 'paid', 'expired')),
    created_at    INTEGER NOT NULL,
    expires_at    INTEGER,
    paid_at       INTEGER,
    closed_reason TEXT,                               -- expired: 'timeout' | 'cancelled'
    watch_until   INTEGER,
    created_by    TEXT    NOT NULL,                   -- the caller's client id: hip/… or user/…
    CHECK ((mode = 'one_time') = (amount_usd IS NOT NULL)),
    CHECK ((mode = 'one_time') = (expires_at IS NOT NULL)),
    CHECK (mode = 'one_time' OR status = 'open'),
    CHECK ((status = 'paid') = (paid_at IS NOT NULL)),
    CHECK ((status = 'expired') = (closed_reason IS NOT NULL))
);
-- Balance mode: one group per customer and network kind.
CREATE UNIQUE INDEX wallet_group_balance ON wallet_group (external_id, testnet)
    WHERE mode = 'balance';
-- One-time: one open intent per external id (an order id).
CREATE UNIQUE INDEX wallet_group_one_time ON wallet_group (external_id, testnet)
    WHERE mode = 'one_time' AND status = 'open';
CREATE INDEX wallet_group_external ON wallet_group (external_id, id);
-- The expiry job's work.
CREATE INDEX wallet_group_expiring ON wallet_group (expires_at) WHERE status = 'open';
CREATE INDEX wallet_group_watch ON wallet_group (watch_until) WHERE watch_until IS NOT NULL;

-- A wallet of a group: a wallet is in at most one group, joins it when it is
-- created, and never leaves (so wallet_event.wallet_group_id never goes stale).
CREATE TABLE wallet_group_member (
    wallet_id       TEXT PRIMARY KEY REFERENCES wallet (id) ON DELETE CASCADE,
    wallet_group_id TEXT NOT NULL REFERENCES wallet_group (id) ON DELETE CASCADE
);
CREATE INDEX wallet_group_member_group ON wallet_group_member (wallet_group_id);
```

- No group delete route: a group's wallets hold keys and money. A cancelled intent is `expired`
  with `closed_reason = 'cancelled'`.
- Member wallets are ordinary `wallet` rows: tag `hashpay`, `external_id` the group's. So the
  wallets API, pipes and triggers (a sweep by tag) see them as any other.
- `wallet_event` gets `wallet_group_id`, kinds `paid`/`expired` and `data.received_usd`: see 103.

## Members and indexes

`POST` builds a group from `accept` (or the `hashpay_accept` setting):

1. **Networks:** every network an accepted asset is on. All must share `testnet`, else `400`.
2. **EVM siblings:** if any EVM network is in (`ethereum`, `bnb`), **every enabled EVM network of
   the same kind joins, with one key**: one `secp256k1` key generated through the first EVM
   network's factory, imported into each (`factory.from_pk`). So `ethereum.mainnet.0xabc` and
   `bnb.mainnet.0xabc` are two wallets, one address.
3. **Other chains:** one generated key per network (Tron, Solana, Bitcoin).
4. **Indexes:** every `network_asset` of every member network, accepted or not, from the tip
   (as `wallets::create_index` with no `start`).
5. **One write:** wallets, indexes, the group, its members. Keys are generated before it, so a
   failure writes nothing. A network with no tip yet: `503`, nothing written.

A mainnet group with ETH, BNB, Tron and Solana accepted:

| Member | Indexes |
|---|---|
| `ethereum.mainnet.0xabc` | ETH, USDT, USDC |
| `bnb.mainnet.0xabc` (same key) | BNB, USDT, USDC |
| `tron.mainnet.TQ…` | TRX, USDT |
| `solana.mainnet.7x…` | SOL, USDC |

10 indexes. Balance mode reuses it; on a later `POST` with a wider `accept`, the missing members
and indexes are added (the same "ensure" code).

## Wrong network, wrong asset

| Mistake | What happens |
|---|---|
| ETH chosen, sent on BNB (or the reverse) | Same address on both: the BNB wallet is indexed; the deposit counts |
| USDT on Ethereum chosen, USDC sent | USDC is indexed too; counts at its USD value, flagged `accepted: false` |
| A non-accepted asset on a member network (BNB when only USDT/BNB is offered) | Indexed (every `network_asset`); counts if priced, flagged |
| A token no `network_asset` lists (e.g. Binance-Peg ETH on BSC) | Not seen: the indexer keeps allowlisted contracts only. The key is kept, so support can recover it by hand |
| A Tron address used on an EVM network | Refused by the sender's wallet: the address formats differ |
| Mainnet funds to a testnet group | Not watched; groups are one kind (`testnet`) |

The SDK warns:

- the network's name, logo and token standard beside the address and under the QR: "Send only
  **USDT** on **BNB Smart Chain (BEP-20)**";
- for an EVM address: "This address works on Ethereum and BNB Smart Chain", to calm the
  commonest fear;
- EIP-681 URIs carry the chain id, so a wallet app switches network or refuses;
- on a deposit whose (network, asset) is not the one selected: an inline notice, "Received
  0.01 BNB on BNB Smart Chain instead: counted as $6.10".

## Payment intent: `POST /v1/wallet_groups`

```json
{
  "external_id": "order-1832",
  "mode": "one_time",
  "amount_usd": "25.00",
  "accept": ["bnb.mainnet.0x55d398326f99059ff775485246999027b3197955",
             "ethereum.mainnet.0xdac17f958d2ee523a2206206994597c13d831ec7",
             "tron.mainnet.TR7NHqjeKQxGTCi8q8ZY4pL8otSzgjLj6t",
             "ethereum.mainnet.native"],
  "ttl": 1800,
  "tolerance_bps": 50
}
```

| Field | Rule |
|---|---|
| `external_id` | 1–128 characters once trimmed (as wallets). The order id for one-time, the customer id for balance |
| `mode` | `one_time` or `balance` |
| `amount_usd` | one-time only, exact decimal > 0, at most 2 fractional digits |
| `accept` | network asset ids; absent: `hashpay_accept` (default: every enabled network's assets of `testnet`) |
| `testnet` | absent: from `accept`; required when `accept` is absent |
| `ttl` | one-time only, seconds, 60 s to 7 days; absent: `hashpay_ttl_s` (1,800) |
| `tolerance_bps` | one-time only, 0–1,000; absent: 50 (0.5%) |

| Case | Answer |
|---|---|
| new | `201` the group and a token |
| balance, the customer has one | `200` that group (missing members added), a fresh token |
| one-time, an open intent of that external id with the same amount and accept | `200` it, a fresh token (a retried checkout) |
| one-time, an open one that differs | `409 already_exists` |

- Scopes: `wallet_groups` and `wallets` (it makes wallets), checked with two
  `require!`s; the token it signs must be covered by the caller, so `wallet_groups:read` too.
- Takes `Caller` (096).

### The intent token: signed by the server

| Option | Verdict |
|---|---|
| **The server signs it in the `POST` answer** | **Chosen.** One round trip; a PHP or WordPress backend needs no JWT code; the scope is always exactly right |
| The third party signs it with `Secret.issue` | Still valid (same scope, same checks), and how a backend refreshes a token offline. Not required |

- Signed **as the caller's issuer**: a key's call → HS256 with the key's `his/…`, `kid` the key
  (the server keeps it readable, 096); a user session → the user's `session_secret`. So
  revoking or rotating the key ends its tokens, as any session.
- Claims: `sub` = the group id; `scopes` =
  `{"com.hashinbox": ["wallet_groups[0193b2c4-7d1e-…]:read"]}`; `iat` now; `exp`: one-time
  `expires_at` + 15 min, balance 1 h; at most 24 h (096).
- **Refresh:** balance: `POST` again (it reuses the group). One-time: `Secret.issue` with the
  same scope, or `POST` again with the same body (the retried-checkout case).

```json
{
  "id": "0193b2c4-7d1e-7c3a-9f00-3c1d2e4f5a6b",
  "external_id": "order-1832",
  "mode": "one_time",
  "status": "open",
  "amount_usd": "25.00",
  "tolerance_bps": 50,
  "testnet": false,
  "accept": ["bnb.mainnet.0x55d3…", "ethereum.mainnet.0xdac1…", "tron.mainnet.TR7N…", "ethereum.mainnet.native"],
  "created_at": 1795600000000,
  "expires_at": 1795601800000,
  "paid_at": null,
  "watch_until": null,
  "wallets": [
    { "id": "ethereum.mainnet.0xabc", "address": "0xabc", "assets": ["native", "0xa0b8…", "0xdac1…"] },
    { "id": "bnb.mainnet.0xabc",      "address": "0xabc", "assets": ["native", "0x55d3…", "0x8ac7…"] },
    { "id": "tron.mainnet.TQ…",       "address": "TQ…",   "assets": ["native", "TR7N…"] }
  ],
  "token": "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCIsImtpZCI6ImhpcC83SHEy…",
  "token_expires_at": 1795602700
}
```

### Other group routes

| Route | Operation id | Scope |
|---|---|---|
| `GET /v1/wallet_groups` `?external_id=` | `wallet_groups_list` | `com.hashinbox.wallet_groups:read` |
| `GET /v1/wallet_groups/{id}` | `wallet_group_get` | `wallet_groups[{id}]:read` |
| `PATCH /v1/wallet_groups/{id}` `{status: "expired"}` | `wallet_group_update` (cancel; phase 2) | `wallet_groups[{id}]` |
| `GET /v1/wallet_groups/{id}/deposits` | `wallet_group_deposits_get` | `wallet_groups[{id}]:read` |
| `GET /v1/wallet_groups/{id}/deposits/stream` | `wallet_group_deposits_stream` | `wallet_groups[{id}]:read` |

- `HASHINBOX` gains `wallet_groups => [read, write]`.
- A list answers `{ wallet_groups, next_cursor }` (`IndexParams`), never a token.

## The deposits endpoint

One response for a group or one index of it, in USD:

| Query | What |
|---|---|
| `index` | a member's `wallet_index_id`: only it (same shape) |
| `since` | Unix ms; only events at or after it (097's `since` → `(since, 0)`). One-time: ignored, everything since the group was made. Balance: absent means the group's life; the SDK sends its token's `iat` |
| `token` | the JWT, GET only (096, for `EventSource`) |

```json
{
  "group": "0193b2c4-7d1e-7c3a-9f00-3c1d2e4f5a6b",
  "mode": "one_time",
  "status": "confirming",
  "amount_usd": "25.00",
  "since": 1795600000000,
  "cursor": "1795600900000-1088",
  "confirmed_usd": "20.00",
  "pending_usd": "5.02",
  "remaining_usd": "0.00",
  "overpaid_usd": "0.00",
  "expires_at": 1795601800000,
  "paid_at": null,
  "indexes": [
    { "wallet_index_id": "bnb.mainnet.0xabc.0x55d398326f99059ff775485246999027b3197955",
      "chain": "bnb", "network": "mainnet", "asset": "0x55d3…", "ticker": "USDT", "decimals": 18,
      "address": "0xabc", "accepted": true,
      "confirmed_usd": "20.00", "pending_usd": "0.00",
      "price_usd": "0.9998", "due": "0", "uri": "ethereum:0x55d3…@56/transfer?address=0xabc&uint256=0" }
  ],
  "deposits": [
    { "wallet_index_id": "bnb.mainnet.0xabc.0x55d3…", "subject": "bnb.mainnet.0x9f…", "txid": "0x9f…",
      "received": "20000000000000000000", "received_usd": "20.00", "estimated_usd": null,
      "state": "confirmed", "confirmations": 15, "required": 15,
      "accepted": true, "late": false, "first_seen_at": 1795600420000, "event": 1061 },
    { "wallet_index_id": "ethereum.mainnet.0xabc.native", "subject": "ethereum.mainnet.0x5e…", "txid": "0x5e…",
      "received": "1600000000000000", "received_usd": "5.02", "estimated_usd": null,
      "state": "pending", "confirmations": 9, "required": 64,
      "accepted": true, "late": false, "first_seen_at": 1795600900000, "event": 1088 }
  ]
}
```

- **Built by folding** the group's events after `since` (`wallet_event_group`), per
  `(wallet_index_id, subject)`: `mined` → pending, `confirmed` → confirmed, `reorged` → gone
  (dropped from `deposits`), `mined` again → pending. Only the received side counts
  (`data.received`, `received_usd`): a sweep or a fee never lowers "deposited".
- `cursor`: the last folded event's cursor (097); the stream's `id:`. A backend that reads the
  deposits can continue the feed from it (`GET /v1/events?group=&cursor=`).
- `remaining_usd` = amount − confirmed − pending (never below 0), so the QR does not ask for more
  while a deposit confirms; `overpaid_usd` = confirmed − amount (never below 0).
- `status` adds one derived value: `confirming` = open, and confirmed + pending reaches the
  amount less tolerance.
- `confirmations` = network tip − position + 1; `required` = the network's `confirmations`.
- `due`: what to send in that asset for `remaining_usd`, atomic, rounded **up** to the asset's
  `display_decimals`; `"0"` when paid; null in balance mode or with no price. `uri` is the QR
  content (below); null with no price in one-time mode (an address-only QR is then the SDK's
  call).
- Model `models/deposits.rs`: `Deposits::fold(group, events, tips, prices)`, pure, tested on its
  own.

### USD rules

| Deposit | Counted toward paid | Shown |
|---|---|---|
| mined or confirmed, priced | its legs' `amount_usd`, recorded when the block was kept (`leg.price`, the median close of the newest hour) | the same |
| mined or confirmed, unpriced (no market price when kept) | 0 | `estimated_usd` at the current price (in-memory candles), or null; `received_usd` null |
| in the mempool (only if 097's open question 1 is ever built) | 0 | `estimated_usd` at the current price, in `pending_usd` |

- **Why recorded, not current:** a mined leg already has `amount_usd`, and the same legs make its
  `confirmed`. The number the user saw as pending is the number that pays: no jump at
  confirmation, and the events feed and the endpoint agree.
- **No price ever:** the deposit stays listed and unpriced; the intent is not paid by it. The
  third party sees it (atomic amounts in the feed) and decides.

### Paid, underpaid, overpaid, late

| Case | Rule |
|---|---|
| Paid | in the write that writes a `confirmed` event of an open one-time group: Σ confirmed `received_usd` ≥ `amount_usd` × (1 − `tolerance_bps`/10⁴) → `status = 'paid'`, `paid_at`, a `paid` event, `watch_until = now + hashpay_watch_days`. Confirmed is never reorged (088), so paid is final |
| Confirmations | the network's (`network.confirmations`: Ethereum 64, BNB 15, Tron 19, Solana 32). No per-intent override in v1 |
| Underpaid | stays open; `remaining_usd` and `due` shrink; the QR shows the rest |
| Expiry | the job, at `expires_at`: confirmed + pending < amount less tolerance → `expired`, `closed_reason = 'timeout'`, an `expired` event. Otherwise it stays open (`confirming`) until its pending deposits confirm (paid) or are reorged away (then the next check expires it) |
| Overpaid | paid; `overpaid_usd` in the `paid` event and the response. Refunds are the shop's |
| Late (after `expires_at` or `paid_at`) | still indexed until `watch_until` (7 days); events as usual, `late: true`; `paid` is never written after `expired`. The shop credits or refunds |
| After `watch_until` | the job pauses the group's indexes (state `paused`): no more scans. Funds sent later stay on the address (the key is kept); resuming an index finds them |
| Balance mode | never paid or expired; `watch_until` NULL; every `confirmed` is a credit |

`paid` event data:

```json
{ "external_id": "order-1832", "mode": "one_time", "amount_usd": "25.00",
  "confirmed_usd": "25.04", "overpaid_usd": "0.04",
  "deposits": [{ "wallet_index_id": "bnb.mainnet.0xabc.0x55d3…", "subject": "bnb.mainnet.0x9f…",
                 "received_usd": "20.00" }, { "…": "…" }] }
```

`expired`: `{external_id, mode, amount_usd, confirmed_usd, pending_usd, reason}`.

### The stream

`GET /v1/wallet_groups/{id}/deposits/stream?since=&index=&token=`, SSE:

- Each message: `event: snapshot`, `id: <cursor>`, `data:` the response above. Whole snapshots,
  not deltas: the component renders the newest, and a reconnect needs no replay
  (`Last-Event-ID` is ignored).
- Sent: on connect; after each commit with an event of this group (the 097 hub, filtered by
  `wallet_group_id`).
- **The 15 s window (103):** the stream ends after about 15 s (or at the token's `exp`, if
  sooner) with `event: reconnect`, `data: {"cursor": null}`; the SDK re-opens it, with a fresh
  token when due. Each window's first snapshot refreshes `confirmations` and `due` (the price),
  so there are no timed snapshots and no keep-alives.
- `x-events: { snapshot: WalletGroupDeposits, reconnect: StreamReconnect }`, so the generated
  clients type it (101).
- The snapshot is for the browser. The same deposits, and the group's `paid` and `expired`,
  reach the backend through the events feed (`?group=`), with cursors (103).

## Back-end verification

The browser's checkmark is never proof. The backend learns and checks with its own key (103):

- **Verify the intent any time:** `GET /v1/wallet_groups/{id}`: on the thank-you page, before
  fulfilling, after any doubt. Its `status` is the truth.
- **The events feed:** `subscribe` (or a 30 s poll of `GET /v1/events?group=…` or
  `?kinds=confirmed,paid,expired`) from its stored cursor; SSE in 15 s windows makes it sooner.
  `paid` and `expired` are group events in the feed (`subject` the group id).
- **Optional nudge:** a trigger with the `webhook` action and `{"events": {"kinds": ["paid"]}}`;
  on a delivery, verify the signature, then `GET` the group.

```ts
const group = await walletGroupGet(api, { id });
if (group.status === "paid") await orders.fulfil(group.external_id);

client.events.subscribe({ cursor: await store.load(), kinds: ["paid", "expired"] }, async (event, ack) => {
  const group = await walletGroupGet(api, { id: event.subject });      // verify, then act
  if (group.status === "paid") await orders.fulfil(group.external_id);
  await store.save(event.cursor);
  ack();
});
```

```go
group, err := hashinbox.WalletGroupGet(ctx, client, hashinbox.WalletGroupGetRequest{ID: id})
if err == nil && group.Status == hashinbox.WalletGroupStatusPaid { fulfil(group.ExternalID) }
```

```php
$group = walletGroupGet($client, new WalletGroupGetRequest(id: $id));
if ($group->status === WalletGroupStatus::Paid) { $order->payment_complete(); }
```

Balance mode credits each `confirmed` from the feed (`subscribe` with `group`, or
`GET /v1/events?group=…&kinds=confirmed`), idempotent by event id, its cursor saved with the
credit (103).

## Security

| An intent token can | It cannot |
|---|---|
| `wallet_group_get`, `wallet_group_deposits_get`, `…_stream` of its group | read another group, list groups, read `/v1/events` |
| see the group's addresses, amounts, status, `external_id` | read the member wallets (`wallets[x]:read` is a different path), send, create, change anything |
| live until its `exp` (≤ 24 h) | outlive its key: revoke or rotate the key and it is 401 |

- No secret ever reaches the browser; keys are never in any answer (as today).
- `external_id` is visible to the browser: never put a secret in it.
- **CORS**, on the three read routes only (the dashboard is same-origin and needs none):
  `Access-Control-Allow-Origin` from `hashpay_origins` (default `["*"]`; a list echoes a
  matching `Origin`), methods `GET`, headers `Authorization, Last-Event-ID`, no credentials
  (the token is the credential), `Max-Age: 600`; preflight answered by the layer.
- **`?token=` in URLs** reaches access logs and `Referer`: the server never logs query strings;
  the page sets `Referrer-Policy: no-referrer` (documented); tokens are short and read-only.
- **Rate limits** (none exist today): an in-memory token bucket in `api/middleware.rs`, keyed by
  the token's issuer and `sub`, else the peer IP: `deposits_get` 2/s (burst 10); at most 3
  streams open at once per token and 50 per IP (a 15 s re-open is not a new one);
  `wallet_groups` `POST` 10/s per key. `429 resource_exhausted` with `Retry-After`.

## QR content

| Network | URI | Notes |
|---|---|---|
| Bitcoin | `bitcoin:bc1q…?amount=0.00041230` (BIP21) | amount in BTC, 8 decimals |
| Ethereum, native | `ethereum:0xabc@1?value=16000000000000000` (EIP-681) | `value` in wei |
| Ethereum, token | `ethereum:0xdac1…@1/transfer?address=0xabc&uint256=25000000` | atomic units |
| BNB Smart Chain | as Ethereum with `@56` (testnet `@97`) | EIP-681 is chain-id based |
| Tron | the address only (`T…`), amount shown as text | no ratified URI standard |
| Solana | `solana:7x…?amount=25&spl-token=EPjF…` (Solana Pay transfer request) | display units |

- EIP-681 token URIs are unevenly supported by wallet apps (some send the native coin to the
  contract). `<hashpay-qr format="address">` offers address-only for tokens; the default stays
  the full URI, open question 8.
- QR rendering: `uqr` (zero dependencies, SVG), bundled.

## The hashpay SDK

### Architecture

```text
@hashinbox/client  (generated + hand-written TS client: Session, stream())
        ▲
@hashpay/core      headless: Checkout state machine, URIs, amounts, formatting
        ▲
@hashpay/elements  Stencil web components (shadow DOM, CSS variables, parts)
        ▲                        ▲
@hashpay/react  @hashpay/vue     generated by Stencil's output targets (Angular later)
```

| Option | Verdict |
|---|---|
| **Stencil** | **Chosen.** Web components with generated React, Vue and Angular wrappers; lazy loading; a hydrate target for SSR |
| Lit | Smaller runtime, but wrappers by hand; React 19 handles custom elements, Vue does too, so it is the fallback if Stencil's build weighs too much |
| React-only components | Excludes WordPress, Vue and plain HTML shops |

### Core (`@hashpay/core`)

```ts
import { Checkout } from "@hashpay/core";

const checkout = new Checkout({
  baseUrl: "https://pay.example.com",
  token: () => fetch("/hashpay/token?order=1832").then((r) => r.text()), // or a string
});
checkout.subscribe((state) => render(state));
await checkout.start();                  // group id from the token's scope (Session.parse)
const offer = checkout.select("bnb.mainnet.0x55d3…"); // { address, due, uri, ticker, network }
checkout.stop();
```

- States: `loading` → `choosing` → `awaiting` → `pending` (seen, confirming n/m) → `paid` |
  `expired`; `error` from any. Balance mode: `awaiting` ⇄ `received` per deposit.
- Streams with `@hashinbox/client`'s fetch-based `stream()` and `Authorization` (no `?token=`
  needed); re-opens at each 15 s window's `reconnect` (103), with backoff on errors and a fresh
  token before `exp`.
- Keeps `since` = the first token's `iat` across refreshes (balance mode).
- Pure helpers, tested alone: `uri(index, due)`, `formatUsd`, `formatAmount(atomic, decimals,
  display)`, `networkLabel`.

### Elements (`@hashpay/elements`)

| Element | Does |
|---|---|
| `<hashpay-checkout>` | the whole flow: select, address and QR, status; owns a `Checkout` |
| `<hashpay-currency-select>` | the accepted assets, grouped by network, with logo and standard (listbox) |
| `<hashpay-qr>` | the URI as an SVG QR; `format="uri" \| "address"` |
| `<hashpay-address>`, `<hashpay-amount>` | text with copy buttons |
| `<hashpay-status>` | awaiting, pending (n/m confirmations, USD so far), the checkmark, expired |
| `<hashpay-progress>` | USD paid of the amount, a bar |
| `<hashpay-timer>` | time to `expires_at` |
| `<hashpay-notice>` | the network warning; a wrong-asset deposit |

```html
<script type="module" src="https://cdn.jsdelivr.net/npm/@hashpay/elements@0.5.0/dist/hashpay/hashpay.esm.js"></script>
<hashpay-checkout base-url="https://pay.example.com" token="eyJ…" locale="de"></hashpay-checkout>
<script type="module">
  const el = document.querySelector("hashpay-checkout");
  el.tokenProvider = () => fetch("/hashpay/token?order=1832").then((r) => r.text());
  el.addEventListener("hashpayPaid", (e) => location.assign("/thanks"));  // UI only; the backend verifies
</script>
```

- Events: `hashpayPaid`, `hashpayExpired`, `hashpayDeposit`, `hashpayError`, `hashpayStateChange`.
- React: `<HashpayCheckout baseUrl={url} token={t} onHashpayPaid={…} />`; Vue:
  `<hashpay-checkout :token="t" @hashpay-paid="…" />`.

### Theming, i18n, accessibility

- **Theming:** CSS variables on the host: `--hashpay-color-primary`, `--hashpay-color-bg`,
  `--hashpay-color-text`, `--hashpay-color-success`, `--hashpay-color-warning`,
  `--hashpay-radius`, `--hashpay-font`, `--hashpay-spacing`; `theme="light" | "dark" | "auto"`
  (`prefers-color-scheme`); `::part(qr)`, `::part(address)`, `::part(status)` for the rest.
- **i18n:** `locale` attribute; bundled `en`, `de`, `es`, `fr`, `cs` (JSON dictionaries, lazy);
  a `messages` property overrides any string; `Intl.NumberFormat` for USD and amounts;
  `dir="rtl"` honoured.
- **Accessibility:** the QR always has its address and amount as text with copy buttons; status
  changes in an `aria-live="polite"` region ("Payment received, confirming", "Paid"); the select
  is a keyboard listbox; visible focus; contrast ≥ 4.5:1 in both themes;
  `prefers-reduced-motion` stills the checkmark; the timer announces only at 5 and 1 minutes.

### Folder and packages: `apps/server/hashpay/`

| Option | Verdict |
|---|---|
| **`apps/server/hashpay/`, beside `clients/`** | **Chosen.** `clients/` is one generated API client per language, written by `build.rs`; hashpay is a product on top of one of them, with its own toolchain (Stencil, npm workspaces) and, later, a PHP plugin. Released on the server's `v*` tag, since it follows its routes |
| `apps/server/clients/hashpay/` | Mixes a UI product into "one folder per language", and puts a Node build under the folder `build.rs` writes into |

```text
apps/server/hashpay/
  package.json                 npm workspaces: core, elements, react, vue
  core/       @hashpay/core      src/{checkout,uri,amount,format,index}.ts, tests/
              depends on @hashinbox/client (file:../../clients/typescript)
  elements/   @hashpay/elements  stencil.config.ts, src/components/hashpay-*/, src/i18n/
  react/      @hashpay/react     written by @stencil/react-output-target
  vue/        @hashpay/vue       written by @stencil/vue-output-target
  examples/   html/, react-vite/, node-backend/ (POST an intent, subscribe to paid/expired)
  wordpress/  (phase 4) hashpay-woocommerce: PHP, on hashinbox/client (PHP) and the elements
```

- **WordPress/WooCommerce (later):** a payment gateway. At checkout it `POST`s a one-time group
  (`external_id` = `wc-{site}-{order}`), renders `<hashpay-checkout>` on the order page with the
  token, and completes the order on `paid`: a WP-Cron job every minute runs `$client->poll()`
  (103) from the cursor it stores in an option, and the order page verifies the group with
  `GET` on return. Optional: a trigger `webhook` to `/wp-json/hashpay/v1/webhook`
  (`Webhook::verify`) runs the poll at once. Settings: server URL, key id and secret, webhook
  secret (optional), accepted assets.

## Sequence

```text
Browser              Shop backend                hashinbox                     Chain
   |-- buy ------------->|                            |                            |
   |                     |-- POST /v1/wallet_groups ->| keys, wallets, indexes     |
   |                     |<- 201 {group, token} ------|  (one write)               |
   |<- page, token ------|                            |                            |
   |-- GET …/deposits/stream (Bearer token) --------->|                            |
   |<- snapshot: open, due per asset -----------------|                            |
   |   picks USDT on BNB, scans the QR ---------------------------------- send --->|
   |                     |                            |<- block kept (indexer) ----|
   |<- snapshot: pending $25, 3/15 -------------------|  mined event               |
   |                     |<- event mined (SSE/poll) --|                            |
   |                     |                            |<- finality moves ----------|
   |<- snapshot: paid --------------------------------|  confirmed + paid (1 write)|
   |   checkmark         |<- events confirmed, paid --|                            |
   |                     |-- GET /v1/wallet_groups/{id} (its key) -->|             |
   |                     |   fulfil                   |                            |
```

## Code placement

| Piece | Where |
|---|---|
| `wallet_group` and its members, their SQL | `models/wallet_group.rs` |
| `Deposits::fold`, the response model | `models/deposits.rs` (pure) |
| create/ensure (keys, wallets, indexes, group in one write), read, deposits, `settle` (paid, called from `usecase/wallet_events.rs` in the same write), expire, pause | `usecase/wallet_groups.rs` |
| expiry and pausing, recurring (`Ack::Again` at the next `expires_at` / `watch_until`, at most 10 s) | `usecase/workers/wallet_groups.rs` |
| routes | `api/routes/wallet_groups.rs`, `api/routes/wallet_group_deposits.rs` |
| CORS layer, rate limiter | `api/middleware.rs` (the routes opt in) |
| token signing | `Session::to_jwt` (096), as the caller's issuer |

- No change to `sdk/base`, `sdk/wallets` or `packages/crypto` is expected: key reuse is
  `factory.generate()` once and `factory.from_pk` per EVM network. If it turns out to need one,
  it comes back for approval first.

## Migration

- `docs/design/104-migrate-1.sql`: `wallet_group`, `wallet_group_member` and their indexes;
  `init.sql` gets the same before `wallet_event`. `wallet_event`'s columns ship with 097 (103).
- Settings: `hashpay_accept` (list of network asset ids; default all), `hashpay_ttl_s` (1,800),
  `hashpay_watch_days` (7), `hashpay_origins` (`["*"]`); all private.

## Tests

- Create: one write; EVM members share an address; every network asset indexed; a mixed
  testnet/mainnet `accept` is 400; balance reuse adds a new network; a retried one-time answers
  200, a different one 409.
- Fold: mined → pending, confirmed, reorged drops it, mined again; sweeps ignored; `since`.
- Paid: reached in the `confirmed` write, `paid` event in it; tolerance; overpay; never twice.
- Expiry: under the amount → `expired`; enough pending → stays open, then paid; pending reorged
  away → expired at the next check. Late deposits flagged. Pausing after `watch_until`.
- USD: recorded values count; unpriced counts 0 with an estimate.
- Token: the `POST`'s token reads its group, 403 on another, 401 after its key is rotated.
- CORS preflight on the three routes only; rate limits answer 429.
- Stream: ends at 15 s with `reconnect`; a re-open gets a fresh snapshot; `paid` and `expired`
  are listed by `GET /v1/events?group=`.
- SDK: core helpers (URIs per chain, rounding up), the state machine over a fake stream; the
  elements in Stencil's e2e (Puppeteer) with a mock server; axe-core on the checkout.

## Plan

| Phase | Scope | Effort |
|---|---|---|
| 0 | 097 and 103 phases 0–2 (events, `(at, id)` cursor, 15 s windows, TS `subscribe`) | see 103 |
| 1 (MVP) | tables, create/ensure, get, list, deposits and stream, paid and expiry, CORS; `@hashpay/core`; `<hashpay-checkout>` (select, QR, status inside), `en` only; HTML and Node examples | 12–15 days |
| 2 | the separate elements, React and Vue wrappers, theming, i18n, rate limits, cancel, pausing, a dashboard groups page | 7–9 days |
| 3 | mempool pending (097 question 1) for Bitcoin and Ethereum; Bitcoin seeded for groups; Angular; sweeps by tag through triggers | 6–8 days |
| 4 | WordPress/WooCommerce plugin | 5–7 days |

## Risks

- **Index growth:** ~10 indexes per one-time intent, kept 7 days past its end. 1,000 orders a
  day ≈ 70,000 live indexes; the per-block cost must be measured before launch. Pausing bounds
  it.
- **Slow checkmarks:** Ethereum's 64 confirmations ≈ 13 min; Bitcoin shows nothing until its
  first block (no mempool). The UI says "confirming n/m"; question 3.
- **Price moves** between the QR and the block: `tolerance_bps` absorbs small ones; a volatile
  asset may underpay, and the QR then asks for the rest.
- **Wallet apps** mis-handling EIP-681 token URIs or chain ids.
- **One EVM key on two chains:** one leaked key loses both. It is the same stored row either way.
- **Sweeping:** funds rest on many addresses; a token sweep needs native gas sent first, and
  that top-up looks like a deposit (question 6).
- **Retention:** 097 prunes events after 90 days, so an old group's deposit list empties; its
  `status` stays on the row.

## Open questions

1. **Unaccepted but indexed assets:** count them toward paid when priced (proposed), or only
   report them?
2. **Stablecoin with no market price:** value USDT/USDC at 1 as a fallback, or leave unpriced
   (proposed)?
3. **Confirmations for paid:** the network's (proposed), or a lower per-intent
   `min_confirmations` (e.g. Ethereum 12) at the shop's risk?
4. **One open one-time intent per external id** (proposed): fine, or allow several (then the
   external id is not the order id)?
5. **Key reuse:** EVM networks only (proposed), or also Tron (same curve)? Reuse on Tron does not
   help mistakes, since the address text differs.
6. **Own transfers:** exclude from deposits a transaction sent by a keyed wallet of this server
   (a gas top-up, a refund loop) (proposed)?
7. **npm scope:** `@hashpay/*` (proposed; must be free) or `@hashinbox/hashpay-*`?
8. **QR for tokens:** the full EIP-681 URI (proposed) or the address only by default?
9. **Per-group revoke:** none in v1 (a key rotation ends every token); add a group `revoked_at`?
10. **Extra BSC assets** (Binance-Peg ETH, BTCB) so mistaken bridged sends are seen?
11. **Watch window:** 7 days after paid or expired (proposed)?
12. **Default accept:** every enabled network's assets, or a curated list (stablecoins plus
    native coins)?
