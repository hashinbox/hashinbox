# 101: Typed API clients, generated from the OpenAPI document

Status: proposed (doc only). Open questions are at the end.

## Problem

- Third parties call the hashinbox API by hand, or run a generic generator over
  `docs/openapi.json`. No generator understands our `security`:
  `"bearer": ["com.hashinbox.wallets[{id}].$read"]` is a scope **template**, filled from the
  request, and checked with `packages/scope`'s coverage rules.
- A client wants to know, before it calls, which scopes a call needs (to sign a narrow key
  session, 096) and whether its session holds them (to hide a button, to fail fast).
- The dashboard's client (`openapi-typescript` + `openapi-fetch`) gives typed paths but no
  scopes, no per-operation functions, and no typed SSE events (`events()` hands out `unknown`).

## Goals and non-goals

- **Goals**
  - Typed clients in several languages (TS, Go, Rust first; Python later), one function or struct
    per `operationId`, carrying its scopes and a `can()` check.
  - One generic generator, `packages/cligen`: OpenAPI 3.1 in, source files out. It knows nothing
    about hashinbox.
  - Each language looks native: its naming, its cancellation, its async, its package manager.
- **Non-goals**
  - **No generated HTTP stack.** cligen defines the interfaces a transport must meet (`Client`,
    `Streamer`, `Session`); the implementations are hand-written once per language, or the
    user's.
  - No request validation (min/max, patterns): the server validates.
  - No retry, rate-limit or reconnect policy in generated code: that is the transport's.
  - Hashinbox-specific parts (scope grammar, key sessions, `?token=`) never live in cligen; they
    live in the hand-written files of `apps/server/clients/<lang>/`.

## Pieces

| Piece | Where | Generic? | Written |
|---|---|---|---|
| Parser → IR | `packages/cligen/src/ir.rs`, `parse.rs` | yes | once |
| Emitters (TS, Go, Rust, Python) | `packages/cligen/src/emit/<lang>.rs` | yes | once per language |
| Interfaces file (`Client`, `Streamer`, `Session`, `Operation`, `ApiError`) | emitted by cligen into each client | yes | generated |
| Models + operations | `apps/server/clients/<lang>/…/gen/` | — | generated |
| `clients` binary | `apps/server/src/bin/clients.rs` | no | once |
| Scope grammar port, sessions, HTTP transport, SSE reader, README, manifest | `apps/server/clients/<lang>/` | no | once per language |
| Scope conformance vectors | `apps/server/clients/vectors/scope.json`, from `packages/scope` | no | generated |

## The intermediate model (IR)

cligen parses the document (a `serde_json::Value`) into a language-neutral IR, then hands it to
one emitter. Emitters never read OpenAPI.

```rust
pub struct Api { pub title: String, pub version: String,
                 pub operations: Vec<Operation>, pub schemas: BTreeMap<String, Schema> }

pub struct Operation {
    pub id: String,                 // "read_wallet", exactly as in the spec
    pub tag: String,                // first tag: "wallets"; untagged → "default"
    pub summary: Option<String>, pub description: Option<String>,
    pub method: Method,             // Get | Post | Put | Patch | Delete
    pub path: PathTemplate,         // "/v1/wallets/{id}" split into literals and holes
    pub params: Vec<Param>,         // path then query, spec order
    pub body: Option<Body>,         // application/json only in v1
    pub success: Success,           // Json(Type) | Empty | Stream(Vec<StreamEvent>)
    pub errors: Vec<(u16, Type)>,   // 400, 401, 403, … in status order
    pub auth: Auth,
}

pub struct Param { pub name: String, pub at: In /* Path | Query */, pub ty: Type,
                   pub required: bool, pub doc: Option<String> }
pub struct StreamEvent { pub name: String /* "message", "missed" */, pub data: Type }

pub enum Auth {
    None,                               // no `security`: public (`read_health`)
    Any(Vec<Requirement>),              // OpenAPI: any requirement object will do
}
pub struct Requirement { pub scheme: String /* "bearer" */, pub scopes: Vec<ScopeTemplate> }
pub struct ScopeTemplate { pub text: String, pub holes: Vec<Hole> } // all of them needed
pub struct Hole { pub name: String, pub param: usize }              // index into `params`

pub enum Type {
    Ref(String), String { format: Option<String> }, Integer { format: Option<String> },
    Number, Boolean, Any,
    Array(Box<Type>), Map(Box<Type>),           // additionalProperties
    Object(Vec<Field>),                         // inline; named ones are Ref
    Enum(Vec<String>),                          // string enums only
    OneOf(Vec<Type>),                           // untagged; tried in order
    Nullable(Box<Type>),                        // ["string","null"], oneOf [null, X]
}
pub struct Field { pub name: String, pub ty: Type, pub required: bool, pub doc: Option<String> }
```

- `bearer: []` (`read_session`, `read_user`) is `Any([Requirement { scopes: [] }])`:
  authenticated, no scope. No `security` is `Auth::None`.
- Within one requirement every scope is needed (OpenAPI AND); several requirements are OR.
  Hashinbox emits exactly one, so `can()` is `all(covers)` in practice.
- `allOf` of objects (`ConfiguredNetwork`, `MarketListing`, `Movement`, `LastRound`) is
  **flattened** into one `Object` at parse time: every language gets a plain struct, no
  inheritance.
- `oneOf [{"type":"null"}, X]` (`NetworkHealth.indexing`, `RpcNode.disabled`) becomes
  `Nullable(X)`; a real `oneOf` (`Listed`: `Setting | SecretSetting`) stays `OneOf`.
- Several 2xx with one schema (`create_wallet` 200 and 201: `Wallet`) are one `Success`.
- Order: operations by tag, then `operationId`; schemas and fields by name. `BTreeMap`
  everywhere, so output is deterministic.

### OpenAPI features in v1

| In | Out (parse error naming the JSON pointer) |
|---|---|
| `$ref` to `#/components/schemas/*` | external `$ref`, `$ref` to parameters/responses |
| `type` arrays with `null`, `oneOf` with `null` | `anyOf`, `not`, `if/then`, `patternProperties` |
| `allOf` of objects (flattened) | `allOf` mixing non-objects |
| `oneOf` untagged; `discriminator` read if present | `discriminator.mapping` to external docs |
| `enum` of strings | non-string enums, `const` |
| `format` `int32`/`int64` (docs and int width) | `format` as validation (`email`, `uuid` are docs only) |
| `additionalProperties` (map), `propertyNames` ignored | tuple `prefixItems` |
| path and query params, scalar or enum `$ref` | header/cookie params, array/object query params |
| `application/json` bodies; `text/event-stream` responses | multipart, form, binary |
| `security` with scope templates; `x-events` | callbacks, webhooks, links |

Unknown keywords not in the table (`minimum`, `default`, `examples`) are kept as doc text or
ignored. Anything *structural* we don't support is a hard error, so a generated client never
silently drops a field.

### Emitter approach: plain `write!` code builders

| Option | Verdict |
|---|---|
| `askama` / `minijinja` templates | A second language in the repo; recursive types (`Nullable(Array(Ref))`) need macros or callbacks into Rust anyway; whitespace control fights every formatter. |
| `quote`-style token builders | Rust-only (`quote`, `genco` for others is one more dependency with its own model). |
| **Plain Rust, one `Emitter` per language, a tiny `Out` writer (`line`, `indent`, `block`)** | **Chosen.** No dependency, type-checked, recursion is just a function (`fn ty(&Type) -> String`), the emitter reads top to bottom like the file it writes. |

```rust
pub trait Emitter {
    /// Every file of the target: relative path → contents.
    fn emit(&self, api: &Api, config: &Target) -> Result<BTreeMap<PathBuf, String>, Error>;
}
```

- The emitter writes formatter-clean code (`gofmt`, `rustfmt`, `prettier` defaults); CI runs each
  formatter's `--check` on the generated files, so drift is caught, not fixed silently.
- Every generated file starts with the language's generated-code marker:
  `// Code generated by cligen from hashinbox-api 0.1.0. DO NOT EDIT.` (the Go form, which
  `gofmt`, golint and GitHub's linguist recognise; the same text in TS and Rust, `#` in Python).

## The interfaces

cligen emits one interfaces file per client (`cligen.ts`, `cligen_gen.go`, `cligen.rs`). It
defines the shape every transport meets. The generated operations only build a neutral
`Request`, hand it over, and decode the `Response`.

| Interface | Does | Implemented by (hashinbox) |
|---|---|---|
| `Client` | sends one `Request`, returns one `Response` (status, body); holds a `Session` | `FetchClient` (TS), `HTTPClient` over the user's `*http.Client` (Go), `ReqwestClient` (Rust) |
| `Streamer` | opens one `Request` as an event stream, yields raw events `{event, data, id}` | fetch-based SSE reader (TS, like today's `events()`), `bufio` reader (Go), `reqwest` + line reader (Rust) |
| `Session` | holds the credential and the scopes; answers `covers(scope)` | `TokenSession`, `KeySession` (see Sessions) |

The neutral request:

```text
Request  { operation: "read_wallet", method: GET, path: "/v1/wallets/ethereum.mainnet.0xabc",
           query: [("cursor","…"), ("limit","100")], body: Option<JSON text> }
Response { status: 200, body: text }
RawEvent { event: "message", data: text, id: Option<text> }
```

- `path` is filled and percent-encoded by the generated code; the transport prefixes its base URL
  and adds the credential. Generated code never sees a header or a URL host.
- **Errors:** a non-2xx answer is `ApiError { status, body }`, `body` the operation's error
  schema (every hashinbox error is `Error`: `kind`, `message`, `reason`, `metadata`). A transport
  failure is passed through as the transport's own error, never wrapped into `ApiError`.
- **Cancellation:** TS `AbortSignal`, Go `context.Context`, Rust dropping the future.

### TypeScript

```ts
export interface Request { operation: string; method: Method; path: string;
  query: [string, string][]; body?: string }
export interface Response { status: number; body: string }
export interface RawEvent { event: string; data: string; id?: string }

export interface Session { covers(scope: string): boolean }
export interface Client {
  readonly session: Session | null;
  send(request: Request, signal?: AbortSignal): Promise<Response>;
}
export interface Streamer {
  open(request: Request, signal?: AbortSignal): AsyncIterable<RawEvent>;
}
export class ApiError<E = unknown> extends Error { status: number; body: E }
```

- A typed stream is an `AsyncIterable` of a discriminated union, one member per `x-events` entry:

```ts
export type StreamLogsEvent =
  | { event: 'message'; data: LogEntry; id?: string }
  | { event: 'missed'; data: LogsMissed; id?: string };
for await (const e of streamLogs(streamer, { level: 'warn' }, signal)) {
  if (e.event === 'missed') console.warn(e.data.missed);
}
```

### Go

```go
type Request struct {
	Operation *Operation // static metadata: ID, Method, Path, Templates
	Method    string
	Path      string     // filled and escaped
	Query     url.Values
	Body      []byte     // JSON, nil when none
}
type Response struct { Status int; Body []byte }
type RawEvent struct { Event, Data, ID string }

type Session interface { Covers(scope string) bool }
type Client interface {
	Session() Session // nil: none
	Do(ctx context.Context, req *Request) (*Response, error)
}
type Streamer interface {
	Open(ctx context.Context, req *Request) (Events, error)
}
type Events interface { Next() (RawEvent, error); Close() error } // io.EOF at the end

type APIError struct { Status int; Body Error }
func (e *APIError) Error() string
```

- The hand-written `hashinbox.New(baseURL, hashinbox.WithHTTPClient(hc), hashinbox.WithSession(s))`
  takes the user's `*http.Client` (timeouts, proxies, mTLS stay theirs); `http.DefaultClient`
  otherwise.
- Typed stream: `Stream[T]` with `Next() (T, error)`; `T` is a struct with `Event string` and
  one pointer field per event (`Message *LogEntry`, `Missed *LogsMissed`), exactly one set.
- `errors.As(err, &apiErr)` for `*APIError`; transport errors come back as returned by `Do`.

### Rust

```rust
pub struct Request { pub operation: &'static str, pub method: Method, pub path: String,
                     pub query: Vec<(&'static str, String)>, pub body: Option<String> }
pub struct Response { pub status: u16, pub body: String }
pub struct RawEvent { pub event: String, pub data: String, pub id: Option<String> }

pub trait Session { fn covers(&self, scope: &str) -> bool; }

pub trait Client: Sync {
    type Error: std::error::Error + Send + Sync + 'static;
    fn session(&self) -> Option<&dyn Session>;
    fn send(&self, request: Request)
        -> impl Future<Output = Result<Response, Self::Error>> + Send;
}
pub trait Streamer: Sync {
    type Error: std::error::Error + Send + Sync + 'static;
    fn open(&self, request: Request)
        -> impl Future<Output = Result<impl Stream<Item = Result<RawEvent, Self::Error>> + Send,
                                       Self::Error>> + Send;
}

pub enum Error<T> { Api { status: u16, body: crate::models::Error }, Decode(serde_json::Error),
                    Transport(T) }
```

- Return-position `impl Trait` in traits (stable since 1.75; the workspace is 1.91): no
  `async-trait` crate, `Send` futures for tokio.
- Typed stream: `impl Stream<Item = Result<StreamLogsEvent, Error<S::Error>>>` with
  `enum StreamLogsEvent { Message(LogEntry), Missed(LogsMissed) }`.

### Python (later)

`typing.Protocol`s with the same members (`send(request) -> Response`, async), models as
`TypedDict`, no dependency but `httpx` in the hand-written transport.

## The generated operation

### Naming

From `operationId` (snake_case in our spec) and schema names (PascalCase):

| | Operation | Params type | Stream event type | Schema | Field |
|---|---|---|---|---|---|
| TS | `readWallet` | `ReadWalletParams` | `StreamLogsEvent` | `Wallet` | `external_id` (wire name, unchanged) |
| Go | `ReadWallet` | `ReadWalletParams` | `StreamLogsEvent` | `Wallet` | `ExternalID` + `json:"external_id"` (initialisms: ID, URL, USD, TOTP) |
| Rust | `ReadWallet` (struct) | the struct itself | `StreamLogsEvent` | `Wallet` | `external_id` |
| Python | `read_wallet` | keyword args | `StreamLogsEvent` | `Wallet` | `external_id` |

- TS keeps wire field names: the type *is* the JSON, no mapping code, no renaming bugs.
- A reserved word (`type`, `match`, `async`) is suffixed `_` (Rust `r#` raw ident instead).
- A name collision after conversion is a generator error, never a silent rename.

### Files

- One file per tag: `wallets`, `networks`, `logs`, `auth`, … (13 today). An operation's file is
  its first tag (`list_tags`, tagged `triggers`, lives in `triggers`).
- One models file holding every schema (107 today), so tag files import from one place and no
  cycle exists. Split per tag later if it grows unwieldy.
- An index file re-exports everything (`index.ts`, `mod.rs`; Go is one package anyway).

### TS: `read_wallet`

```ts
// src/gen/wallets.ts
// Code generated by cligen from hashinbox-api 0.1.0. DO NOT EDIT.
import { type Client, type Session, send } from './cligen';
import { escapeId } from '../scope';
import type { Wallet } from './models';

export interface ReadWalletParams {
  /** `{chain}.{network}.{address}`, as a create call returned it. */
  id: string;
}

/** Read one wallet's public metadata. `GET /v1/wallets/{id}` */
export function readWallet(client: Client, params: ReadWalletParams, signal?: AbortSignal): Promise<Wallet> {
  return send<Wallet>(client, readWallet.request(params), signal);
}
readWallet.id = 'read_wallet' as const;
readWallet.method = 'GET' as const;
readWallet.path = '/v1/wallets/{id}' as const;
readWallet.templates = ['com.hashinbox.wallets[{id}].$read'] as const;
readWallet.request = (params: ReadWalletParams) => ({
  operation: 'read_wallet', method: 'GET' as const, query: [],
  path: `/v1/wallets/${encodeURIComponent(params.id)}`,
});
readWallet.scopes = (params: ReadWalletParams): string[] => [
  `com.hashinbox.wallets[${escapeId(params.id)}].$read`,
];
readWallet.can = (session: Session, params: ReadWalletParams): boolean =>
  readWallet.scopes(params).every((scope) => session.covers(scope));
```

- `listNetworks.scopes()` and `listNetworks.can(session)` take no params (no hole).
- `body`: a body op takes it as `params.body` (`createWallet(client, { body })`); a param named
  `body` is a generator error.
- `escapeId` is the runtime hook named in the target's config (see `clients` binary); with none
  configured, a hole is substituted as is.

### Go: `read_wallet`

```go
// ReadWalletParams are the parameters of ReadWallet.
type ReadWalletParams struct {
	// ID is `{chain}.{network}.{address}`, as a create call returned it.
	ID string
}

// ReadWalletOperation is GET /v1/wallets/{id}.
var ReadWalletOperation = Operation{ID: "read_wallet", Method: "GET", Path: "/v1/wallets/{id}",
	Templates: []string{"com.hashinbox.wallets[{id}].$read"}}

// Scopes are the scopes ReadWallet needs for p.
func (p ReadWalletParams) Scopes() []string {
	return []string{"com.hashinbox.wallets[" + EscapeID(p.ID) + "].$read"}
}

// Can reports whether s holds every scope ReadWallet needs for p.
func (p ReadWalletParams) Can(s Session) bool { return coversAll(s, p.Scopes()) }

// ReadWallet reads one wallet's public metadata.
func ReadWallet(ctx context.Context, c Client, p ReadWalletParams) (*Wallet, error) {
	return send[Wallet](ctx, c, &Request{Operation: &ReadWalletOperation, Method: "GET",
		Path: "/v1/wallets/" + url.PathEscape(p.ID)})
}
```

- Scopes and `Can` are methods on the params struct: the scopes depend on the params, and the
  struct exists for every op (`ListNetworksParams{}` is empty).

### Rust: `read_wallet`

```rust
/// Read one wallet's public metadata: `GET /v1/wallets/{id}`.
#[derive(Clone, Debug)]
pub struct ReadWallet {
    /// `{chain}.{network}.{address}`, as a create call returned it.
    pub id: String,
}

impl Operation for ReadWallet {
    type Output = Wallet;
    const ID: &'static str = "read_wallet";
    const METHOD: Method = Method::Get;
    const PATH: &'static str = "/v1/wallets/{id}";
    const TEMPLATES: &'static [&'static str] = &["com.hashinbox.wallets[{id}].$read"];

    fn scopes(&self) -> Vec<String> {
        vec![format!("com.hashinbox.wallets[{}].$read", escape_id(&self.id))]
    }
    fn request(&self) -> Request {
        Request::new(Self::ID, Self::METHOD, format!("/v1/wallets/{}", encode(&self.id)))
    }
}

// `send` and `can` are provided methods of `Operation`:
let wallet = ReadWallet { id: id.clone() }.send(&client).await?;
let allowed = ReadWallet { id }.can(session);
```

- A struct per op (not a free fn): the params, the consts and the response type are one value a
  caller can store, log or batch. Optional query params are `Option<T>` fields with
  `#[derive(Default)]`, so `ListLogs { level: Some("warn".into()), ..Default::default() }`.

## Scopes

### What the spec carries: `security` is enough

| Option | Verdict |
|---|---|
| **`security` as today + one rule: each `{name}` in a template names a path parameter of that route** | **Chosen.** No new spec surface; Swagger UI and other tools show the same text; the rule is generic (cligen documents it, any project can use it). |
| `x-scopes: [{template, holes: {id: {in: path, name: id}}}]` | Only needed when a hole comes from the body (`packages/scope/tests/utoipa.rs`'s `from = transfer.from`). No real route does that today. |

- Today the template keeps the Rust variable name (`require!(caller, "…wallets[{id}]…", id =
  path.id)`); that it matches the path param `id` is luck. `#[scope::utoipa]` already reads
  `path = "/v1/wallets/{id}"`, so it **checks at compile time** that every hole names a `{…}` of
  the path, and fails to compile otherwise.
- cligen fails generation on a hole that names no parameter. A body-filled hole becomes
  `x-scopes` the day one is needed (open question 6).
- The scope grammar itself is not in the spec. cligen treats a scope as opaque text with holes;
  meaning is the `Session`'s.

### Resolving

- `scopes(params)` substitutes each hole with the param's value, passed through the target's
  `escape` hook (hashinbox: `%`, `[`, `]`, whitespace, controls as `%XX`, `packages/scope`'s
  `escape`). `ethereum.mainnet.0xabc` stays as is.
- No hole: `scopes()` is the templates, constant.

### Coverage: `can()`

- Generated: `can(session, params) = Auth::Any(reqs).any(|r| r.scopes(params).all(session.covers))`.
  `Auth::None` → `true`; `bearer: []` → `session` present.
- `Session.covers(scope)` is hand-written per language: a port of `Scope::parse`,
  `Scope::covers`, `Scopes::unpack`, `pack`, `compress` and `escape` (about 150 lines each).
  Rust uses `packages/scope` itself, no port.
- **Conformance vectors** keep the ports honest: a test in `packages/scope`
  (`cargo test -p scope vectors`, `BLESS=1` to rewrite) writes
  `apps/server/clients/vectors/scope.json` from the crate's own answers:

```json
{ "parse":   [{ "text": "com.hashinbox.wallets[a,b%5D%20c%25d].$*", "ok": true }],
  "refuse":  ["", "$read", "wallets[].$read", "wallets[%zz].$read"],
  "covers":  [{ "ours": "a.*.$read", "theirs": "a.wallets[x].legs.$read", "covers": true }],
  "pack":    [{ "scopes": ["a.wallets[x].$read", "a.markets.$read"],
                "packed": { "a": ["markets.$read", "wallets[x].$read"] } }],
  "escape":  [{ "id": "a,b] c%d", "text": "a,b%5D%20c%25d" }] }
```

- The cases are the crate's existing test cases (`coverage`, `bad_text_is_refused`,
  `ids_are_escaped`, `compress_drops_covered`) moved into one table both sides read. CI fails if
  the file is stale; each language's test suite runs every vector.

## Sessions and keys per language

Hand-written, in `apps/server/clients/<lang>/session.*`, the same three things everywhere:

| | Does | Scopes from |
|---|---|---|
| `TokenSession(jwt)` | sends `Bearer <jwt>` | the JWT's `scopes` claim, unpacked; **not verified** (the client cannot; the server does) |
| `KeySession.sign({keyId, secret, sub?, scopes, ttl})` | signs an HS256 JWT (096) and is a `TokenSession` | the scopes it signed |
| `SecretSession(secret)` | sends `Bearer his/…` | `GET /v1/user` → `key.scopes`, read once (`load()`), else unknown (open question 2) |

Signing, per 096: header `{alg: HS256, typ: JWT, kid: hip/…}`; claims `iss` (= `kid`), `sub?`,
`iat`, `exp` (≤ 24 h), `scopes` packed (`Scopes::pack`); HMAC key = the whole `his/…` text.
No JWT library: base64url + HMAC-SHA256 from the platform.

| Language | HMAC from | Signature |
|---|---|---|
| TS | WebCrypto `crypto.subtle` (browsers, Node ≥ 18, Deno, Bun) | `await KeySession.sign({...})`: async, WebCrypto is |
| Go | `crypto/hmac`, `crypto/sha256` | `SignKeySession(KeyClaims) (TokenSession, error)` |
| Rust | `hmac` + `sha2` | `KeySession::sign(&claims) -> TokenSession` |
| Python | `hmac`, `hashlib` | `KeySession.sign(...)` |

- One TS package for browser and Node: WebCrypto removes the need for a separate `node` client.
  A browser never holds `his/…`; it gets a narrow JWT from its own server (096 need 3).
- `session.scopes()` returns the unpacked list; `session.expiresAt` from `exp`, so a transport
  can refresh before it lapses.
- **Streams and `?token=`:** the default `Streamer` is fetch-based and sends the header. A
  user who wants `EventSource` passes `{ tokenInQuery: true }`; only the transport appends
  `?token=<jwt>` (never a `his/…` secret), and only for GET (096 open question 7).

## SSE in the spec: `x-events`

The stream routes answer `text/event-stream` with `schema: {type: string}`: nothing to type.
The server adds one vendor extension on that media type:

```json
"text/event-stream": {
  "schema": { "type": "string" },
  "x-events": {
    "message": { "$ref": "#/components/schemas/LogEntry" },
    "missed":  { "$ref": "#/components/schemas/LogsMissed" }
  }
}
```

- `stream_logs`: `message` → `LogEntry`, `missed` → `{missed: n}` (a new `LogsMissed` schema).
- `search_tip`: `probe`, `found`, `error` (new `TipProbe`, `TipFound`, `TipError` schemas).
- 097's `/v1/wallets/{id}/events/stream`: one entry per kind, each the envelope.
- Written by a `utoipa::Modify` beside `Bearer` in `api.rs`, from a `const EVENTS` next to each
  stream handler, so the names sit with the code that sends them.
- OpenAPI 3.2's `itemSchema` covers the same; cligen reads it too once utoipa emits 3.2.
- No `x-events`: cligen still emits the op, yielding `RawEvent`s.

## The `clients` binary

```sh
cargo run -p hashinbox --bin clients -- --out apps/server/clients            # every target
cargo run -p hashinbox --bin clients -- --out apps/server/clients --only ts,go
cargo run -p hashinbox --bin clients -- --check                              # CI: exit 1 if stale
```

- `apps/server/src/bin/clients.rs`, about 60 lines: read the document, for each
  `<out>/<lang>/cligen.toml` run its emitter, write (or with `--check`, diff) the files, and
  delete generated files no longer emitted (only files carrying the marker).
- **The document:** `docs/openapi.json` (`--spec` to override), not `api::openapi()`.

| Option | Verdict |
|---|---|
| Call `api::openapi()` | Needs a `src/lib.rs`: the crate is binary-only (`mod api;` … in `main.rs`, tests mounted inside it). Moving nine modules and widening visibility is a refactor of its own. |
| **Read `docs/openapi.json`** | **Chosen.** It is the committed contract and equals `hashinbox --openapi`; CI checks that first, then `clients --check`. |
| `hashinbox --clients` flag on the server binary | Mixes a dev tool into the shipped binary. |

- One config file per target, next to the code it describes:

```toml
# apps/server/clients/js/cligen.toml
language = "typescript"
out = "src/gen"                 # relative to this file; cligen owns this directory
runtime = "../scope"            # import of the hand-written hooks
[scopes]
escape = "escapeId"             # hole filter; absent: substitute as is
[naming]                        # defaults per language; overrides only
initialisms = ["ID", "URL", "USD", "TOTP"]   # Go
```

- Determinism: sorted IR, no timestamps (the marker names the spec's `info.version`, not a
  date), `\n` endings, trailing newline. Two runs give the same bytes.

## Layout of `apps/server/clients/`

```text
apps/server/clients/
  vectors/scope.json                  generated by packages/scope
  js/                                 npm: @hashinbox/client
    cligen.toml  package.json  tsconfig.json  README.md
    src/gen/      cligen.ts models.ts wallets.ts networks.ts … index.ts   (generated)
    src/scope.ts  session.ts  fetch.ts  sse.ts  index.ts                  (hand-written)
    test/
  go/                                 module path: open question 1
    cligen.toml  go.mod  README.md
    cligen_gen.go  models_gen.go  wallets_gen.go …                        (generated)
    scope.go  session.go  http.go  sse.go                                 (hand-written)
  rust/                               crates.io: hashinbox-client; workspace member
    cligen.toml  Cargo.toml  README.md
    src/gen/      mod.rs cligen.rs models.rs wallets.rs …                 (generated)
    src/lib.rs  session.rs  reqwest.rs  sse.rs                            (hand-written)
  python/                             later; PyPI: hashinbox
```

- Generated files live where the language expects them (`src/gen/`, Go's `_gen.go` suffix, since a
  Go package is one directory).
- **Tests**, per client:
  - unit: every scope vector; `can()`/`scopes()` of a few ops; request building against golden
    `Request`s; decoding recorded bodies;
  - **conformance** (phase 3): one scripted scenario list (`conformance/cases.json`: sign a key
    session, create a wallet, read it, 403 on another wallet, stream logs) run against a server
    started by `hashinbox serve` on a temp DB, the same cases in every language.
- **Versioning:** a client's version is the server's (`workspace.package.version`, `info.version`);
  released on the same `v*` tag. A breaking spec change is a breaking client change.
- **Publishing** (later): npm, crates.io (needs `scope` and `scope-macros` published, or the
  client depending on `scope` with the macros behind a feature), the Go module tag
  (`apps/server/clients/go/v0.1.0` for a subdirectory module), PyPI. One CI job per registry on
  `v*`.

## The dashboard (`apps/server/ui`)

| Option | Verdict |
|---|---|
| **Move to `@hashinbox/client` (path dependency `file:../clients/js`)** | **Chosen, phase 2.** One TS client in the repo; `listNetworks.can(session)` replaces scope checks by hand; typed SSE replaces `events()`'s `unknown`. |
| Stay on `openapi-typescript` + `openapi-fetch` | Two TS clients drift; no scopes. |

- `client.ts` shrinks to the token and 401 handling, now a `FetchClient` with `onUnauthorized`;
  `call(client.GET('/v1/networks'))` becomes `listNetworks(api)`.
- Kept hand-written: `exchange()` (the docs console sends arbitrary requests) and `openapi()`.
- `npm run api` and `schema.d.ts` go; the `Schemas['Wallet']` aliases become imports from the
  package. Page by page, both clients coexist until the last page moves.

## Plan

| Phase | Scope | Effort |
|---|---|---|
| 1 | `packages/cligen`: parser, IR, TS + Go + Rust emitters (models, operations, `scopes`, `can`, interfaces file); `#[scope::utoipa]` hole check; `scope.json` vectors; scope ports (TS, Go); `TokenSession`/`KeySession`; transports (fetch, `net/http`, `reqwest`); `clients` binary; CI `--check` + formatter checks | 6–8 days |
| 2 | SSE: `x-events` in the server, typed streams, `Streamer` impls; dashboard migration | 3–4 days |
| 3 | Conformance suite against a live server; Python client | 3–4 days |
| 4 | Publishing: npm, crates.io, Go module, PyPI; READMEs with the 096 signing examples per language | 2 days |

## Risks

- **Grammar drift** between `packages/scope` and its ports: vectors, regenerated by the crate's
  tests, checked in CI.
- **Go `oneOf`** (`Listed`) has no native union: a struct with one pointer per variant and an
  `UnmarshalJSON` trying them in order. Fine for one schema; ugly if they multiply.
- **`int64` in TS:** `number` loses precision above 2^53. Our int64s are positions and Unix ms;
  amounts are strings. Safe today, a trap if an id becomes a large integer.
- **Enum growth:** a server adding `JobState` values must not break old Rust/Go clients:
  generated enums carry an `Unknown(String)` / unmatched-string fallback.
- **Generator scope creep:** the features table is the contract; new OpenAPI features enter only
  with a route that needs them.
- **Spec quality is now API:** a vague schema (`Trigger.action` as `object`) becomes `any` in
  every language. Tightening it is a client-visible change.

## Open questions

1. **Go module path:** a subdirectory module of this repository, or a mirror repository
   (`github.com/…/hashinbox-go`) so `go get` paths stay short?
2. **`SecretSession`:** a key's scopes are not in `his/…`. Load them with `GET /v1/user` on
   first use (proposed), or have clients always sign a short key session and never send the
   secret?
3. **`?token=`** for `EventSource`: keep it a transport option (proposed), or drop it and support
   fetch-based streams only?
4. **TS integers:** `number` (proposed) or `bigint`/string for `int64`?
5. **Errors:** one `ApiError` with `Error` for every status (proposed, the spec has one error
   schema), or a per-status union per operation?
6. **Body-filled scope holes:** never (routes check bodies with unnarrowed scopes, like
   `create_triggers` with `wallets.$write`), or add `x-scopes` when the first one appears?
7. **Rust client and `scope`:** depend on the crate (publish `scope` + `scope-macros`), or a
   `scope` feature `macros` off by default?
8. **`src/lib.rs` for the server:** worth splitting later, so `clients` calls `api::openapi()`
   and drops the `docs/openapi.json` hop?
9. **cligen's home:** stays in this workspace, or its own repository once a second project uses
   it?
