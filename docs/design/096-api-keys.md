# 096: Users, keys, scopes and sessions

Status: proposed (doc only). It replaces the single bearer token (`bearer_token_env`,
`http_server::protected_router`). Scopes come from `packages/scope`. Open questions are at the
end.

## Needs

1. **A third party's server** calls the API with a secret: it creates wallets, reads them, sends.
2. **The dashboard** logs a user in with email, password (or the recovery passphrase) and,
   optionally, a TOTP code.
3. **A browser "pay" form** gets a token, signed by the third party's server, that can only read
   one wallet's balance and events.

## Concepts

| Term | What it is |
|---|---|
| **User** | A person or team owner. Holds the widest scopes anything they issue may have. |
| **Key** | Issued by a user: public client id `hip/…` and secret `his/…`. Its scopes are covered by its user's. |
| **Session** | An HS256 JWT, issued by a user (the server signs it at login) or by a key (its secret signs it, offline, with any JWT library). Its scopes are covered by its issuer's. |
| **Scope** | `com.hashinbox.wallets[<id>]:read`, as `packages/scope` defines and checks it. |

**Scope chain:** user ⊇ key ⊇ session. Each step is checked with `Scopes::first_uncovered`, both
sides expanded (Dependencies, below). It is
checked when something is issued, and again on every request, so narrowing a user narrows their
keys and tokens at once.

### Scope text

```text
scope   = step *("." step) [":read"]
step    = name [ "[" id "]" ]
name    = 1*(ALPHA / DIGIT / "_" / "-")
id      = any text; "%", "[", "]", whitespace and controls written as %XX
```

- **Two levels.** `com.hashinbox.triggers` is **full** (read and write);
  `com.hashinbox.triggers:read` is **read only**. Full covers `:read` of the same path.
- **One id per step.** `wallets[w1]`, `wallets[w1]:read`; several wallets are several scopes.
- **A scope covers its whole subtree.** `networks` covers `networks.endpoints`,
  `networks[x].endpoints[y]` and so on; the level carries (`networks:read` covers
  `networks.endpoints:read`, not `networks.endpoints`). A child never covers its parent; ids
  narrow (`wallets` covers `wallets[x]`, `wallets[x]` only `wallets[x]`). `com.hashinbox` is
  everything.
- **No wildcards.** The subtree rule gives what a trailing `*` gave, and nothing needs a
  wildcard in the middle of a path.
- **Compress** drops each scope another covers, then sorts. It merges nothing.
- **Pack** compresses, then factors out the shared prefix: `{"com.hashinbox": ["triggers",
  "wallets[w1]:read"]}`. Each scope keeps one step at least, so the root alone packs as
  `{"com": ["hashinbox"]}`. **Unpack** joins each prefix and its scopes with `.`.

### The scope tree

`auth::HASHINBOX` declares it (`scope::schema!`); `HASHINBOX.knows` refuses a scope outside it.
Every route requires exactly one scope (`require!`), the most specific. A list route needs the
collection's `:read` (`wallets:read`); a per-id scope gives per-id routes only. Assets and market
reads (`/v1/assets`, `/v1/markets`, `/v1/markets/{name}/candles`) need only a session.

| Scope | Allows | Routes |
|---|---|---|
| `com.hashinbox` | everything | every route |
| `wallets` | create wallets; with `:read` list them | `POST /v1/wallets`; `GET /v1/wallets` |
| `wallets[id]` | one wallet and its subtree; `:read` reads it | `GET /v1/wallets/{id}` (`:read`) |
| `wallets[id].indexes` | its indexes: create, change, remove, reconcile; `:read`: their legs | `POST /v1/wallets/{id}/indexes`, `PATCH`/`DELETE /v1/wallets/{id}/indexes/{asset}`, `POST …/{asset}/reconcile`; `GET …/{asset}/legs` (`:read`) |
| `wallets[id].tags` | tag and untag it | `PUT`/`DELETE /v1/wallets/{id}/tags/{tag}` |
| `wallets.tags:read` | list every tag in use | `GET /v1/wallets/tags` |
| `wallets[id].transactions` | send from it | `POST /v1/wallets/{id}/transactions` |
| `networks` | create networks; `:read`: list them, their blocks and tip | `POST /v1/networks`; `GET /v1/networks`, `GET /v1/networks/{chain}/{network}/blocks`, `…/blocks/{position}` |
| `networks.transactions:read` | a network's and a block's transactions | `GET /v1/networks/{chain}/{network}/transactions`, `…/blocks/{position}/transactions` |
| `networks.endpoints` | RPC nodes (their URLs carry API keys): add, replace, test, remove, switch; `:read`: list them, their failures | `POST`/`PUT /v1/networks/{chain}/{network}/endpoints`, `POST …/endpoints/test`, `POST /v1/networks/endpoints`, `DELETE …/endpoints/{id}`, `PUT …/endpoints/{id}/state`; `GET …/endpoints`, `GET …/endpoints/{id}/failures` (`:read`) |
| `networks.routes` | which node serves which call; `:read` reads it | `PUT`; `GET /v1/networks/{chain}/{network}/routes` |
| `triggers` | create, replace, delete triggers; `:read` reads them | `POST /v1/triggers`, `PUT`/`DELETE /v1/triggers/{id}`; `GET /v1/triggers`, `GET /v1/triggers/{id}` |
| `triggers.runs:read` | a trigger's runs | `GET /v1/triggers/{id}/runs` |
| `jobs:read` | the job queue | `GET /v1/jobs`, `GET /v1/jobs/{id}` |
| `logs:read` | the kept log entries | `GET /v1/logs`, `…/targets`, `…/messages`, `…/stream` |
| `markets` | turn a market's feed on or off | `PATCH /v1/markets/{name}` |
| `stats:read` | what the store holds | `GET /v1/stats` |
| `users` | create users, change their scopes, disable them; `:read` lists them | `POST /v1/users`, `PATCH /v1/users/{id}`; `GET /v1/users` |
| `users.keys` | the caller's keys: create, revoke, rotate; `:read` lists them | `POST /v1/user/keys`, `DELETE /v1/user/keys/{id}`, `POST …/{id}/rotate`; `GET /v1/user/keys` |
| `settings` | write a setting; `:read` lists them | `PUT /v1/settings/{key}`; `GET /v1/settings` |
| `recovery` | the recovery passphrase: read whether it is set, set, remove | `GET`/`POST`/`DELETE /v1/recovery` |

Scopes are written in full (`com.hashinbox.wallets`); the table leaves out `com.hashinbox.`.

### Dependencies

Some scopes need others outside their subtree. `auth::DEPENDS` lists them, source → targets;
the hierarchy needs no entry (a parent covers its children).

| Source | Targets | Why |
|---|---|---|
| `triggers` | `wallets` | a trigger's send broadcasts from the wallets its selector picks |
| `wallets` | `jobs` | a send is followed as its broadcast job |
| `networks.endpoints` | `networks:read` | nodes are added to a network: it must be seen |
| `networks.transactions` | `networks:read` | transactions are browsed by network and block |
| `networks.routes` | `networks.endpoints:read` | a route picks among the network's nodes |

- **The level carries.** A source held full gives a bare target full; held `:read`, the target
  `:read`. A target written `:read` is always read only. So `triggers:read` reads wallets and
  jobs; `triggers` writes them.
- **Ids.** A scope covering the source or an id of it triggers it (`triggers[t1]`,
  `networks[x].endpoints`, `networks`); a child (`triggers.runs`) does not. The target is
  granted whole.
- **Transitive**, to a fixed point: `triggers` gives `wallets`, which gives `jobs`.

### Expansion

`Scopes::expand(DEPENDS)` adds every target, compressed.

- **Stored scopes stay as granted.** The middleware expands the session's scopes once, before
  any `require!`; `GET /v1/session` answers the expanded ones.
- **Narrowing compares expanded against expanded:** a session asked of a user or key, a key or
  user created, and every request's chain check (`User::accepts`, `UserKey::accepts`).
- **Clients expand the same.** The OpenAPI document carries the table as the top-level
  extension `x-hashinbox-depends` (source → targets); cligen generates it as `DEPENDS` (Go:
  `Depends`) into each client's operations file, and `Session.parse` expands with it, so `can()`
  matches the server. `_golden/scope.json` holds the coverage, packing and expansion cases each
  client port passes.

### Migration

`docs/design/096-migrate-2.sql` rewrites the stored scopes (`user.scopes`, `user_key.scopes`)
once: `X.$read` → `X:read`; `X.$write`, `X.$*`, `X.*` → `X`; `endpoints` → `networks.endpoints`
and `networks.routes`; `transactions` → `networks.transactions`; `keys` → `users.keys`;
`assets` and `markets:read` are dropped (no scope needed), as is anything else it cannot map,
each listed. The root user holds `com.hashinbox`. A session issued before it no longer parses:
401, log in again.

## Tables

```sql
CREATE TABLE user (
    id            TEXT    PRIMARY KEY,         -- uuid v7
    email         TEXT    NOT NULL UNIQUE,
    name          TEXT    NOT NULL,
    password_hash TEXT,                        -- argon2id; NULL: no dashboard login
    scopes        TEXT    NOT NULL,            -- JSON, Scopes::pack form
    session_secret BLOB   NOT NULL,            -- 32 random bytes: signs the user's sessions (HS256)
    created_at    INTEGER NOT NULL,
    invalidated_at INTEGER,                    -- every session issued at or before it: 401
    disabled_at   INTEGER                      -- disabled: every key and session refused
);

CREATE TABLE user_key (
    id           TEXT    PRIMARY KEY,          -- hip/…: the key's public client id
    user_id      TEXT    NOT NULL REFERENCES user (id) ON DELETE CASCADE,
    name         TEXT    NOT NULL,             -- "shop backend"
    secret_hash  BLOB    NOT NULL UNIQUE,      -- SHA-256 of his/…: bearer lookup
    secret       TEXT    NOT NULL,             -- his/…, readable: the HS256 key of the key's sessions
    scopes       TEXT    NOT NULL,             -- JSON, packed; covered by user.scopes
    created_at   INTEGER NOT NULL,
    expires_at   INTEGER,
    invalidated_at INTEGER,                    -- every session it issued at or before it: 401
    revoked_at   INTEGER,                      -- the key itself refused, its sessions too
    last_used_at INTEGER
);
CREATE INDEX user_key_user ON user_key (user_id);

CREATE TABLE user_2fa (
    user_id      TEXT    PRIMARY KEY REFERENCES user (id) ON DELETE CASCADE,
    secret       BLOB    NOT NULL,             -- TOTP secret (RFC 6238)
    confirmed_at INTEGER,                      -- NULL until the first valid code
    last_step    INTEGER,                      -- step of the last code used: no replay
    created_at   INTEGER NOT NULL
);
```

- Scopes are stored as the JSON `Scopes::pack` makes (`{"com.hashinbox": ["wallets"]}`),
  the same shape the token carries. `Scopes::unpack` reads them, and `HASHINBOX.knows` refuses
  an unknown scope when one is written.
- The server keeps `his/…` readable in `secret` (plain, like the TOTP secret; decided), because
  checking an HMAC needs it, and its SHA-256 in `secret_hash`, which finds the key from a
  `Bearer his/…` request. See Signing for the trade-off.
- A user's `session_secret` never leaves the server: only the server signs and verifies user
  sessions.
- `user_2fa.confirmed_at` stays NULL until the user enters a first valid code, so a half-finished
  setup never locks them out.

## Keys

```text
hip/7Hq2Wk9xR4mLpZ3vN8cT1a                 public client id: 128 random bits, base58
his/3xT9…Kq2x7Hq2W                         secret: base58 of 32 random bytes ‖ their CRC32
```

- Both read `<kind>/<id>`: `hip/…` public, `his/…` secret. The prefixes let secret scanning and
  our logs recognise a leaked secret.
- Base 58 is Bitcoin's alphabet (`crypto::base58`, no check bytes): no `0`, `O`, `I` or `l`, and
  nothing a URL or a double-click splits. Its length varies with the number (a leading zero
  byte is a `1`): an id is about 22 characters, a secret about 49.
- The secret is one base-58 number of 36 bytes: the 32 random bytes, then their CRC32 (4 bytes,
  big-endian). The CRC is our server's typo check only: it refuses a mistyped secret before any
  lookup. A third party never computes it; it signs with the secret as given.
- A key is created by its user (a session, or a key holding `users.keys`). Its scopes must be
  covered by the creator's, and the secret is shown once.
- `Authorization: Bearer his/…` authenticates as the key: a session with the key's scopes, still
  covered by its user's.

## Sessions: the JWT is the session

Every JWT is a session. There are two kinds, told apart by who issued it. The JWT names its
issuer by a **public client id** in `kid` and `iss`:

| | Issued by the user | Issued by a key |
|---|---|---|
| Client id | `user/0193…` (the user id) | `hip/7Hq2…` (the key id) |
| Signed with | HS256, the user's `session_secret`, held only by the server | HS256, the key's secret text `his/…` exactly as given (the server keeps it to verify) |
| Made by | `POST /v1/session` with email, password and TOTP code | the third party, offline (or `POST /v1/session` with `Bearer his/…`) |
| `sub` | the user id | anything the third party wants (its customer); logged, grants nothing |
| `scopes` | the user's, or fewer if asked | what the third party asks, covered by the key's |

```json
// header
{ "alg": "HS256", "typ": "JWT", "kid": "hip/7Hq2Wk9xR4mLpZ3vN8cT1a" }
// claims
{
  "iss": "hip/7Hq2Wk9xR4mLpZ3vN8cT1a",
  "sub": "customer-1832",
  "iat": 1795600000,
  "exp": 1795600900,
  "jti": "0193…",                 // optional
  "scopes": { "com.hashinbox": ["wallets[ethereum.mainnet.0xabc]:read"] }
}
```

### Signing: HS256 for both kinds

Both kinds are HS256 JWTs: a user's with its `session_secret`, a key's with its secret text, the
whole `his/…` string as the HMAC key, exactly as given (no decoding, no checksum strip).

- **Why:** a third party signs with any JWT library, in any language, and the plain secret it
  was shown: HMAC-SHA256 only, which every library and runtime has, and no key format.
- **Trade-off:** the server must keep the secret readable (`user_key.secret`) to check the HMAC.
  A leak of the database lets an attacker forge that key's sessions (within its scopes), as well
  as use it as a bearer. It is stored plain by decision, like the TOTP secret; revoke or rotate
  the key after a leak.
- `jsonwebtoken` encodes and decodes. `Session::from_jwt` decodes without the signature (it
  only reads `kid` and the claims); the middleware then verifies with the issuer's key, the
  algorithm pinned to HS256, so `alg: none`, `EdDSA` or any other algorithm is refused.
- Our checks only: the 24-hour maximum, `iat` at most 60 s in the future, `exp`, invalidation,
  revoked, expired, disabled, and the scope chain. `jsonwebtoken`'s own `exp`, `nbf`, `aud` and
  required-claim checks are off (leeway 0), so there is one clock and one rule.

### Node.js: signing a key's session

With the `jsonwebtoken` npm package:

```js
import jwt from "jsonwebtoken";

const keyId = "hip/7Hq2Wk9xR4mLpZ3vN8cT1a";
const secret = process.env.HASHINBOX_SECRET; // "his/…", as shown when the key was created
const scopes = { "com.hashinbox": ["wallets[ethereum.mainnet.0xabc]:read"] };

const token = jwt.sign({ sub: "customer-1832", scopes }, secret, {
  algorithm: "HS256",
  keyid: keyId,
  issuer: keyId,
  expiresIn: "15m",
});
```

With no dependency, `node:crypto`:

```js
import { createHmac } from "node:crypto";

const keyId = "hip/7Hq2Wk9xR4mLpZ3vN8cT1a";
const secret = process.env.HASHINBOX_SECRET; // "his/…"
const part = (value) => Buffer.from(JSON.stringify(value)).toString("base64url");

const now = Math.floor(Date.now() / 1000);
const header = part({ alg: "HS256", typ: "JWT", kid: keyId });
const claims = part({
  iss: keyId,
  sub: "customer-1832",
  iat: now,
  exp: now + 15 * 60,
  scopes: { "com.hashinbox": ["wallets[ethereum.mainnet.0xabc]:read"] },
});
const signature = createHmac("sha256", secret).update(`${header}.${claims}`).digest("base64url");
const token = `${header}.${claims}.${signature}`;
```

- Send it as `Authorization: Bearer <token>`, or `?token=` on a GET.
- Claims are fixed: `iss` (the key id, as `kid`), `iat`, `exp` (at most 24 hours after `iat`),
  `scopes`, and optionally `sub` and `jti`. Any other claim is refused.

### The `Session` model

`models/session.rs`: the data a JWT carries, and nothing else. It holds no table and no issuer,
and needs no input but the JWT.

```rust
pub(crate) enum Client {
    User(UserId),                 // user/…
    Key(UserKeyId),               // hip/…
}

pub(crate) struct Session {
    pub(crate) client: Client,    // kid / iss: who issued it
    pub(crate) sub: Option<String>,
    pub(crate) scopes: Scopes,
    pub(crate) issued_at: i64,    // iat
    pub(crate) expires_at: i64,   // exp
    pub(crate) id: String,        // jti
    jwt: Option<String>,          // the JWT it was decoded from, for the issuer to check
}

impl Session {
    /// The session `jwt` holds: decoded, `exp` in the future and `exp − iat` at most 24 hours.
    /// The signature is not checked here: only its issuer holds what checks it.
    pub(crate) fn from_jwt(jwt: &str, now: i64) -> Result<Self, Invalid>;
    /// Its JWT, HS256 with `key` (a user's `session_secret`, or a key's `his/…` text).
    pub(crate) fn to_jwt(&self, key: &[u8]) -> String;
    /// Whether `key` (the user's `session_secret`, or the key's `secret`) signed it.
    pub(crate) fn is_signed_by(&self, key: &[u8]) -> bool;
}

```

### The checks, one rule each

The one middleware (see One middleware) hands the token to
`usecase::sessions::authenticate(db, token, now) -> Result<Caller, Error>`. It picks one short
function per kind of token; each reads the issuer in one `db.read`, then asks the models.

| Token | Function | Reads | Checks, each a model's method |
|---|---|---|---|
| `his/…` | `by_secret` | the key by the secret's SHA-256, and its user | `KeySecret::parse` (checksum); the session built plainly from the key (`client: hip/…`, the key's scopes, `issued_at = now`, signed by no one); `key.accepts`, `user.accepts` |
| JWT, `kid: user/…` | `by_user_session` | the user; when was the recovery passphrase set, for a `passphrase` login | `Session::from_jwt` (HS256, `exp`, 24-hour maximum, `iat` at most 60 s ahead); `session.is_signed_by(user.session_secret)`; `user.accepts` |
| JWT, `kid: hip/…` | `by_key_session` | the key and its user; the recovery passphrase's time, as above | `Session::from_jwt`; `session.is_signed_by(key.secret)`; `key.accepts`, `user.accepts` |

- `UserKey::accepts(session, now)`: not revoked or expired, its `invalidated_at` before `iat`,
  session ⊆ key.
- `User::accepts(session, key, recovery_set_at)`: not disabled, its `invalidated_at` before
  `iat`, a `passphrase` session's `iat` after the recovery passphrase was last set (see Recovery
  passphrase), session ⊆ user and key ⊆ user.
- A malformed or forged token, or one whose key or user is not kept: `401`, no reason. A genuine
  one no longer accepted (expired, invalidated, revoked, disabled, narrowed): `401`
  `session_expired`.
- **Invariant:** a `Session` a handler sees was checked by all of the above.

### `Caller`: what a handler checks scopes against

The middleware puts one value in the request: who is calling, and the scopes that count
(`models::Caller`).

```rust
pub(crate) struct Caller {
    pub(crate) user: User,
    pub(crate) key: Option<UserKey>,
    pub(crate) session: Session,
}

impl scope::Granted for Caller {
    /// The scopes that count: the session's.
    fn scopes(&self) -> &Scopes { &self.session.scopes }
}
```

The scopes that count are the key's when a key is calling, otherwise the user's, and narrowed
further by the session when the session asks for less:

| Caller | `session.scopes` is | so it is |
|---|---|---|
| `Bearer his/…` | the key's (`by_secret` builds the session from them) | the key's |
| a session a key signed (the pay form) | what the third party put in it, checked ⊆ key ⊆ user | at most the key's; less when narrowed |
| a user session (dashboard) | what the login asked for, checked ⊆ user | at most the user's; less when narrowed |

- Using `key.scopes` directly would be wrong for the pay form: a token narrowed to
  `wallets[x]:read` would get everything its key holds.
- `authenticate` has already checked session ⊆ key ⊆ user, so `session.scopes` is always the
  narrowest of the three, and the only one a handler compares.
- The middleware only parses and checks. Whether a route needs authentication is the route's
  business (see below).

### Reading the user and key

Each authenticated request reads its user (and key) by primary key or unique hash:
`user::db::find(connection, id)`, `user_key::db::find(connection, id)` and
`user_key::db::find_by_secret_hash(connection, hash)`, in one `db.read` when it needs both.
Login reads its user with `user::db::find_by_email`. Nothing is kept in memory, so revoke and
invalidation take effect on the next request. The one cache is `root_exists`, an `AtomicBool`
in `AppState` (see Bootstrap).

### Invalidation

| Action | Sets | Effect |
|---|---|---|
| log out everywhere (`DELETE /v1/session?all=true`) | `user.invalidated_at = now` | every session the user and their keys issued until now: 401 |
| password changed (`PUT /v1/user`), 2FA turned off | `user.invalidated_at = now` | the same; `PUT /v1/user` answers a fresh session issued the next second, which it spares |
| key rotated | `user_key.invalidated_at = now` | every session the key issued until now: 401; the key keeps working |
| key revoked | `user_key.revoked_at = now` | the key (bearer `his/…`) and every session it issued: 401 for good |
| user disabled | `user.disabled_at = now` | the user, their keys and every session: 401 |

- The comparison is "issued at or before": a session issued in the same second as the
  invalidation is refused too, because `iat` has only second precision.
- **Cost:** none beyond the per-request read of the user and key, so it takes effect on the
  next request.

## Endpoints

### `/v1/session`: the session is the JWT

| Method | Auth | Body | Answer |
|---|---|---|---|
| `POST` | none | `{email, password, totp?, scopes?, ttl?}` or `{email, passphrase, totp?, scopes?, ttl?}` | `201 {token, expires_at, user, scopes}` |
| `POST` | `Bearer his/…` | `{sub?, scopes, ttl?}` | `201 {token, expires_at, scopes}`: a presigned token signed on the server, for a caller that cannot sign |
| `GET` | `Bearer <jwt>` | – | `200 {kind, user, key?, sub, scopes, issued_at, expires_at, amr}`: the token decoded and checked |
| `DELETE` | `Bearer <jwt>` | `?all=true` | `204`: alone, the client forgets the token; with `all`, sets the issuer's `invalidated_at` (every session until now: 401) |

- A login sends exactly one of `password` or `passphrase` (the recovery passphrase, see Recovery
  passphrase); both or neither is `400`.
- A login with 2FA confirmed and no `totp` answers
  `401 {"kind": "unauthenticated", "reason": "totp_required", …}`. The dashboard then asks for
  the code and posts again.
- A wrong password, passphrase, unknown email or wrong code all answer the same `401` after the
  same delay, so an attacker cannot tell which part was wrong: `reason` `wrong_credentials`. An
  unknown email's password is checked against a decoy hash (`crypto::password::DECOY`); a
  passphrase is checked against the recovery hash (the decoy when none is set) whatever the
  email.
- A user session carries the standard `amr` claim (RFC 8176): `["pwd"]` or `["passphrase"]`,
  then `"otp"` when a code was used. A key's session has none. `GET /v1/session` shows it, and
  `auth.login.ok` / `auth.login.failed` log the `method` (`pwd` or `passphrase`, never the
  words).
- A genuine session no longer accepted (expired, logged out everywhere, its key revoked or
  rotated) answers `401` with `reason` `session_expired`; a malformed or forged one has no
  `reason`.
- `scopes` narrows the session or token (for example, a read-only dashboard tab). `ttl` is at most
  24 hours.

### Users, keys, 2FA

| Route | Scope | What |
|---|---|---|
| `GET /v1/user` | any session | the logged-in user and their scopes (singular: always the caller, like `/v1/session`) |
| `PUT /v1/user` | any session of the user (a key: `403`) | `{password}`: sets a new password, checked as at signup (8 characters at least); the current one is not asked. Sets `invalidated_at` (every other session ends) and answers `200` with a fresh session (the caller's scopes and `amr`). It replaces `PUT /v1/user/password` |
| `GET /v1/version` | public | `{version}`, plus `missing_root: true` on a fresh install (no user yet); also the health check |
| `GET /v1/recovery` | `recovery` (`Caller`) | `200 {set, created_at}` (`created_at` Unix ms, `null` when none is set); never the words or the hash |
| `POST /v1/recovery` | `recovery` (`Caller`) | `201 {passphrase: "w1 … w12"}`, shown once; replaces any passphrase set and ends its sessions |
| `DELETE /v1/recovery` | `recovery` (`Caller`) | `204`: removes it (none set: `204` too); passphrase logins are off and its sessions end |
| `GET /v1/users`, `POST /v1/users`, `PATCH /v1/users/{id}` | `users:read` / `users` (`POST` with no user yet: public, creates root) | list, create (scopes covered by the creator's), change scopes, disable |
| `GET /v1/user/keys`, `POST /v1/user/keys`, `DELETE /v1/user/keys/{id}`, `POST /v1/user/keys/{id}/rotate` | `users.keys:read` / `users.keys` | the logged-in user's own keys: list (never secrets), create (secret shown once), revoke, rotate |
| `POST /v1/user/2fa` | session | start: answers the `otpauth://` URI for the QR code (unconfirmed) |
| `POST /v1/user/2fa/confirm` | session | `{code}`: sets `confirmed_at` |
| `DELETE /v1/user/2fa` | session plus `{code}` | turns 2FA off |

## Endpoints in code: authentication, then authorization

There are two parts. The middleware **authenticates**: it says who is calling. The endpoint
**authorizes**: it says what the call needs. Each scope is written once, and it reaches both the
OpenAPI spec and the runtime check.

### The route decides whether it needs authentication

The one middleware (see One middleware) runs the checks above. With no token it does nothing;
with a bad one it answers `401`.

Whether a route needs authentication is the route's own business, stated by what it takes:

| Handler takes | No `Authorization` | Use |
|---|---|---|
| `caller: Caller` | `401` (the extractor finds none) | every route that reads or changes data |
| `caller: Option<Caller>` | runs with `None` | a route that answers more when someone is logged in |
| none of them | runs | public: health, `openapi.json`, `POST /v1/session` with email and password |

- The protected/public router split goes away: one router, one middleware. A route is
  protected because its handler takes `Session` or `Caller` and calls `scope::require!`.
- A sent but bad credential is refused even on a public route, so a client never mistakes a
  dead key for anonymous access.
- `Caller` carries who is calling (`user`, `key`) and the scopes that count (`scope::Granted`).

### Endpoint: `scope::require!` checks, `#[scope::utoipa]` reads it

```rust
#[scope::utoipa(get, path = "/v1/wallets/{id}", responses((status = 200, body = Wallet)))]
async fn read(session: Session, Path(path): Path<WalletPath>, …) -> Result<Json<Wallet>, Error> {
    scope::require!(session, "com.hashinbox.wallets[{id}]:read", id = path.id);
    …
}
```

A check needs two inputs:

| Input | Comes from |
|---|---|
| **needs** (required) | the template, a string literal: `"com.hashinbox.wallets[{id}]:read"` |
| **has** (granted) | the first argument: any type implementing `scope::Granted` (`Session`, `Caller`) |

`scope::require!(granted, "template", name = expr, …)` (a proc macro in crate `scope-macros`, folder `packages/scope/macro`,
re-exported by `scope`):

- `{name}` takes the variable `name` in scope, or the named argument `name = expr`, as `format!`
  does. Each value is `Display`.
- Each value is **escaped** as an id (`%`, `[`, `]`, whitespace as `%XX`) before it goes in,
  so an id cannot change the scope's structure: `w1].x[w2` stays one odd id.
- The filled text is parsed by `scope::Scope`'s parser. Text that does not parse is a bug and
  is denied, never a panic.
- When `granted` does not cover it, it returns `Err(Denied.into())`. `Denied` converts to
  `403 {"kind": "permission_denied", "reason": "missing_scope", "metadata": {"scope": "…"}}`.
- Every route writes exactly one, its most specific scope. What a scope needs beyond its
  subtree is a dependency (`auth::DEPENDS`), not a second `require!`: `POST /v1/triggers` needs
  only `com.hashinbox.triggers`, which brings `wallets`. A template holds one id per step.

`#[scope::utoipa(<utoipa::path args>)]` reads the handler's body. It writes every `require!`
template, in order, deduplicated, exactly as written, into `#[utoipa::path(…,
security(("bearer" = ["com.hashinbox.wallets[{id}]:read", …])))]`. It fails to compile when the
body has no `require!`, when `security(…)` is given, or when a template is not a string literal.

- **Write `require!` in the handler itself.** The attribute only sees the handler's own body: a
  `require!` in a function it calls still checks at run time, but OpenAPI does not show it.

`scope::Granted` keeps the package generic: it knows nothing of users, keys or JWTs.

```rust
pub trait Granted {
    fn scopes(&self) -> &Scopes;
}
impl Granted for Scopes { fn scopes(&self) -> &Scopes { self } }   // tests, plain checks
```

### Lists

A list route requires the unnarrowed scope. It does not narrow its query to granted ids:

```rust
#[scope::utoipa(get, path = "/v1/wallets", …)]
async fn list(session: Session, …) {
    scope::require!(session, "com.hashinbox.wallets:read");
    …
}
```

- `GET /v1/wallets` needs `com.hashinbox.wallets:read`.
- A caller granted only `wallets[x]:read` is refused the list; it reads `GET /v1/wallets/x`.

### What is checked where

| Check | When |
|---|---|
| `#[scope::utoipa]` without `require!`, with `security(…)`, or a non-literal template | compile time |
| a template's text parses | every request (`require!` denies one that does not) |
| every OpenAPI `security` scope is known (`HASHINBOX.knows`) | test |
| every route in the OpenAPI document but the public ones answers `401` with no `Authorization` and `403` to a session with no scopes (a route that forgot `Session` or the attribute fails it) | test (the same idea as `tests/events.rs`) |
| the caller holds each scope the handler requires | every request (`require!`) |

- Public routes (health, `openapi.json`) take no `Caller` and have no attribute.
- The server's schema (`scope::schema!`, `HASHINBOX`) is the scope tree. It is the source for
  `knows`; the dashboard's picker reads the tree from the OpenAPI document's `security`.

## API layout

The `api` module is regrouped so the auth pieces have a place. It is a pure move: handlers,
paths and the OpenAPI document stay as they are.

```text
src/api.rs              router(), openapi(), Contract, Error, ErrorBody, IndexParams  (today api/mod.rs)
src/api/state.rs        AppState (unchanged; gains root_exists)
src/api/middleware.rs   the one middleware, token_of, the Session and Caller extractors
src/api/routes.rs       declares the route modules; one router merging them all
src/api/routes/*.rs     one file per resource, as today:
    assets, health, jobs, logs, markets, networks,
    network_blocks, network_block_transactions, network_endpoints,
    network_endpoint_routes, network_transactions, openapi, session, stats,
    triggers, user, users, keys, wallets, wallet_indexes, wallet_index_legs,
    wallet_tags, wallet_transactions
```

- `src/api.rs` next to the `src/api/` folder, not `api/api.rs`, which would be the module
  `api::api` (clippy's `module_inception`). There is no `mod.rs`.
- `routes/` stays flat: one file per resource, with today's names.
- The protected/public split (`protected_routes`, `public_routes`) goes away. One middleware
  covers every route; a route file attaches nothing.
- Route files import `crate::api::{Error, IndexParams, AppState}` in place of `super::…`.
- `http_server::protected_router` keeps only the body limit. The bearer check moves to
  `middleware.rs`.

## One middleware

One middleware covers the whole router, public routes too: `api::router` applies
`axum::middleware::from_fn_with_state(state, middleware::authenticate)` with the real `AppState`.
`openapi()` builds the routes without state and never runs it.

1. No token (no `Authorization`, and no `?token=` on a GET): it puts nothing, and continues.
2. A bad token (malformed, forged, expired, invalidated, revoked, disabled): `401` at once, on
   any route, so a client never mistakes a dead key for anonymous access.
3. A good token: verified once (`usecase::sessions::authenticate`), its `Caller` (`Session`,
   `User` and `Option<UserKey>`) put in the request.

The handler picks what it takes:

| Handler takes | No token | Use |
|---|---|---|
| nothing | runs | public: health, `openapi.json`, login |
| `session: Session` | `401` `bearer_required` | routes whose scopes are all they check (market prices, a wallet's balance) |
| `caller: Caller` | `401` `bearer_required` | every write, and routes that need who is calling (`GET /v1/user`, creating a key) |
| `Option<Session>`, `Option<Caller>` | runs with `None` | a route that answers more when someone is logged in |

- **A JWT is never trusted unchecked**: `Session` and `Caller` read what the middleware checked,
  never the token. Both cost the same read.
- `Session` and `Caller` both implement `scope::Granted`, with the session's scopes, so
  `scope::require!` works with either.
- **Every write takes `Caller`.** A `POST`, `PUT`, `PATCH` or `DELETE` (wallets, indexes, tags,
  sends, endpoints, routes, triggers, market feeds, settings, users, keys) takes `caller: Caller`.
  Read-only `GET` routes may take `Session`.
- **Fails closed:** the guard test (every non-public route answers `401` with no `Authorization`,
  `403` to a session with no scopes) catches a route that forgot `Session` or `require!`.

## Recovery passphrase

A BIP39 seed for the instance: 12 English words (128 bits of entropy and the 4-bit SHA-256
checksum, `crypto::mnemonic`). It signs in as any user when a password is lost.

- **Hash only:** the secret setting `recovery_passphrase` keeps the argon2id hash of the
  normalised words (lower case, single spaces), never the words.
- **Optional, the root's:** none is set after setup. The root creates, replaces or removes it at
  any time (`/v1/recovery`, all three on full `recovery`, which only the root holds, through
  `com.hashinbox`; reading whether it is set needs it too, so the scope has no `:read` use).
  - `POST` makes 12 new words, keeps their hash in place of any one set (`setting::put`) and
    answers them this once. `GET` says `{set, created_at}`, never the words. `DELETE` drops the
    row (`setting::db::delete`).
  - With none set, a passphrase login answers `401 wrong_credentials` after the same decoy hash
    check, like any wrong secret.
  - Logs: `auth.recovery.created`, `auth.recovery.replaced`, `auth.recovery.removed` (the
    `user`, never the words).
- **Replacing or removing ends its sessions:** a session with `passphrase` in its `amr` is
  refused (`401 session_expired`) when no passphrase is set, or when its `iat` is in or before the
  second the passphrase row was last written (its `updated_at`). The middleware reads that one
  column only for such a session, in the read that finds its user, and checks it beside the
  user's `invalidated_at` (`Session::is_ended_by_recovery`). No other state is kept.
  - A login's session is issued as of the read that found the hash it checked, so a login in
    flight while the words are replaced is refused too.
  - `POST` answers the new words only once the second they were set in is over, so no session
    they make is issued in that second (which would be refused).
- **Any account:** `POST /v1/session {email, passphrase, totp?}` works for every user's email.
  `parse` takes any spacing or case and checks the words and checksum; text that is not a
  mnemonic is still hashed against the decoy, so it answers like a wrong one.
- **2FA still applies:** a user with 2FA on needs the code (`totp_required`, then `totp`), as with
  a password. The session's `amr` is `["passphrase"]`, plus `"otp"`.
- After signing in, `PUT /v1/user {password}` sets a new password without the old one.

> **Warning:** whoever holds the 12 words can sign in as **any** account that has no 2FA. Write
> them down offline; anyone who reads them owns every such account.

### Secret settings

`settings!` declares each key `public`, `private` or `secret`. A secret one:

- is never returned by any route: `GET /v1/settings` lists it as `{key, set, description}` only;
- is never writable through `PUT /v1/settings/{key}`: `403 permission_denied` (a caller holding
  `settings` is refused by the declaration, not by a missing scope);
- is set only by server code (`setting::put`, which replaces any row kept).

## Bootstrap: the first user

With no users and no keys there is nobody to authenticate, so `POST /v1/users` has two modes:

| State | `POST /v1/users {email, name, password}` |
|---|---|
| **no user exists** | needs no `Authorization`; creates the **root** user with `com.hashinbox` and answers `201` with the user and a session (logged in at once) |
| a user exists | needs a `Caller` with `users`; the new user's scopes must be covered by the caller's (`first_uncovered`), else `403` |

- **Atomic:** the "no user exists" check and the insert are one write, so two first requests
  racing make one root, and the other answers `403`.
- `GET /v1/version` (public) answers the server's version. On a fresh install it also says so:

  ```json
  { "version": "0.1.0", "missing_root": true }
  ```

  - `missing_root` is present only while no user exists. Once the root is created it is left
    out, not sent as `false`.
  - `root_exists` is cached as an `AtomicBool` in `AppState`, set at start by
    `SELECT EXISTS(SELECT 1 FROM "user")` (`user::db::any`). While it is false, each call runs
    that query again and sets it once a user exists. The usecase that creates the root sets it
    after its write commits, so the next call no longer shows `missing_root`.
  - It is also the health check: a `200` means the server is up and serving. It joins the
    existing `/health` route (kept for load balancers).
  - **`version` is the git tag, fixed at build time.** `apps/server/build.rs` (which already
    embeds the UI) runs `git describe --tags --always --dirty` and passes the result to the
    compiler (`cargo:rustc-env=HASHINBOX_VERSION=…`). The server reads it as a constant:

    ```rust
    /// The git tag this binary was built from: `v0.4.0`, `v0.4.0-3-g1a2b3c4` (3 commits
    /// after it), `-dirty` with uncommitted changes.
    pub(crate) const VERSION: &str = env!("HASHINBOX_VERSION");
    ```

    - Without git (a source tarball, a Docker build without `.git`), it falls back to
      `CARGO_PKG_VERSION` (`0.1.0`).
    - A `HASHINBOX_VERSION` set in the build environment wins over both, so CI and Docker pass
      the release tag in.
    - `cargo:rerun-if-changed=.git/HEAD` and `.git/refs` rebuild it when HEAD moves or a tag
      is made.
    - The binary prints it too: `hashinbox --version`, and the `app.start` log line.
  - The dashboard reads it to show "create the root user" instead of the login form.
- **The setup flow** the dashboard walks, two steps from `GET /v1/version`:
  1. **Root** (`missing_root`): `POST /v1/users` with no `Authorization`.
  2. **Organization:** the instance's name, the `instance` setting (`PUT /v1/settings/instance`).

  The recovery passphrase is not a step: the root creates it later, if at all (see Recovery
  passphrase).
- **Preset (`serve --config`, `HASHINBOX_CONFIG`):** a deployment skips the setup flow. The
  value is inline JSON (starts with `{`) or a JSON file's path,
  `{"root": {email, name, password}, "settings": {key: value}}`, both optional,
  `deny_unknown_fields` (`usecase::bootstrap`).
  - No user: in one write, the root is created as `POST /v1/users` creates it (same checks,
    `com.hashinbox`), then every setting written as `PUT /v1/settings/{key}` writes it;
    `recovery_passphrase` is 12 BIP39 words (`Mnemonic::parse`), kept as the argon2id hash
    `POST /v1/recovery` keeps. `root_exists` is set; `app.bootstrap` logs the email and the keys.
  - No user and no `root`: the server does not start (settings without a root are refused).
  - A user exists: the config is not read; nothing is overwritten.
  - Any problem (unreadable file, bad JSON, unknown field, bad email or password, bad setting,
    bad mnemonic) stops the start, naming the field, never echoing the password or the words.
- **Risk:** until the root exists, whoever reaches the server first becomes root. The server
  binds loopback by default (`http_server` refuses a public bind without TLS), so this is
  someone on the machine. See the open questions for a setup code.
- The env bearer token (`bearer_token_env`) is removed (see the open questions).

## Dashboard

### Login (`/ui/login`)

- **First run** (`GET /v1/version` → `missing_root`): a form for email, name, password and repeat
  password. It posts `POST /v1/users`, stores the returned session and opens the dashboard.
- **Otherwise:** email and password (or the recovery passphrase), then `POST /v1/session`.
  - On `401 totp_required`, a second step asks for the 6-digit code and posts again with
    `totp`.
  - A wrong email, password or code all show "email, password or code is wrong".
- The session (the JWT) is kept in `sessionStorage`, as the bearer token is today. When it
  expires, or any call answers `401`, the dashboard goes back to the login page.
- It replaces the Lock dialog.

### Users page (`/ui/users`)

The same layout as every page: the sidebar (`Side` / `SideItem`) lists users (name, email; dot:
green active, red disabled, grey no login). The detail has four sections:

| Section | Shows | Does |
|---|---|---|
| **Profile** | name, email, scopes (each with what it allows, from `HASHINBOX`), created | edit name and email; change password (`PUT /v1/user`, the new one only; logs out everywhere else); for another user with `users`: change scopes, disable |
| **Keys** | each key: name, `hip/…`, scopes, created, last used, expires; dot green active, orange expiring, red revoked | create (name, scopes from a picker limited to the user's, expiry): the `his/…` secret is shown **once** with a copy button; rotate (invalidates its sessions); revoke |
| **Two-factor** | off, pending (not confirmed) or on, since when | turn on: QR code (`otpauth://`), then a code to confirm; turn off (needs a code) |
| **Sessions** | this session: issued, expires, scopes | log out; log out everywhere (`DELETE /v1/session?all=true`) |

- The logged-in user's own page is `/ui/users/me`, from the top bar (name and avatar next to
  the logo), so even a user without `users:read` reaches their profile, keys and 2FA. Under the
  hood it calls `GET /v1/user`.
- The scope picker shows the scope tree with a Read / Full choice per node, offers only what
  the user holds (expanded), notes what a node implies (`also wallets`), and allows ids
  (`wallets[…]`). It shows the packed result before saving.
- Every action is logged like the rest (`auth.*` events), and the Logs button opens them.

## Pieces

| Piece | Where |
|---|---|
| scope grammar, `schema!`, `pack`, `expand`, `Granted` | `packages/scope` |
| `require!`, `#[scope::utoipa]` | `packages/scope/macro` (proc-macro crate `scope-macros`: `syn`, `quote`), re-exported by `scope` |
| `HASHINBOX` schema (`auth.rs`), key format (`auth/key.rs`) | `apps/server/src/` |
| password hashes, TOTP, BIP39 mnemonic, base 58, SHA-256, CRC32, OS randomness | `packages/crypto` (`password`, `totp`, `mnemonic`, `base58`, `sha256`, `crc32`, `random`) |
| `user`, `user_key`, `user_2fa` models | `apps/server/src/models/` (one file each) |
| `Session`, `Client` (the JWT's data, no table) | `apps/server/src/models/session.rs` |
| the middleware, `Session` and `Caller` extractors | `apps/server/src/api/middleware.rs` |
| `Caller` | `apps/server/src/models/caller.rs` |
| `authenticate`, `by_secret`, `by_user_session`, `by_key_session` | `apps/server/src/usecase/sessions.rs` |
| routes | `api/routes/{session,user,users,keys}.rs` |
| dashboard | `ui/src/pages/login/`, `ui/src/pages/users/` (profile, keys, two-factor, sessions); the Lock dialog removed |

New dependencies: `jsonwebtoken` (both kinds of session, HS256) in the server. The purely
cryptographic pieces live in `packages/crypto` (approved), with `argon2`, `bitcoin_hashes`
(TOTP's HMAC-SHA1) and `crc`; it also had `bs58`, `sha2` and `rand_core` (OS randomness)
already:

| Module | Public API |
|---|---|
| `crypto::password` | `hash(&str) -> Result<String>` (argon2id PHC), `verify(&str, &str) -> bool`, `DECOY` |
| `crypto::totp` | `code(secret, step)`, `verify(secret, typed, now, last_step) -> Option<step>` (±1 step, after `last_step`), `uri(secret, issuer, account)`, `STEP`; base 32 is private |
| `crypto::mnemonic` | `Mnemonic::{generate, from_entropy, parse}`, `Display`, `Invalid`, `WORDS`; the English wordlist `include_str!`-ed |
| `crypto::base58` | `encode`, `decode` |
| `crypto` | `random::<N>()`, `sha256`, `crc32` |

## Open questions

1. **Client id spelling:** done. Every id reads `<kind>/<id>`: `user/…`, `hip/…`, and the secret
   `his/…`.
2. **The TOTP secret is stored readable** (codes are computed from it). Encrypt it with a server
   key, or keep it plain like the rest of the DB?
3. **Recovery codes** for a lost phone: in v1, or later? A lost password is covered by the
   recovery passphrase; a lost phone still is not (2FA applies to the passphrase too).
4. **The env bearer token:** remove it after bootstrap, or keep it as a break-glass root?
5. **The table name `user`** is a reserved word in Postgres. Keep it (SQLite only), or call it
   `account`?
6. **The `scopes` claim** is a custom JSON object (packed). The standard `scope` claim is a
   space-separated string. Use the custom one only (proposed, it is shorter), or both?
7. **Pay-form token in the URL** (`?token=`, so `EventSource` works, design 097): allow it on GET
   routes only?
8. **First-user setup code:** the server prints a one-time code in its log at start while no
   user exists, and `POST /v1/users` needs it for the root. It closes the "first to reach the
   server" race on a server bound beyond loopback. Add it?
