# 101: Typed API clients, generated from the OpenAPI document

Status: phase 1 generator built (`packages/cligen`, `apps/server/build.rs`, the starting
files of `apps/server/clients/`). Open questions are at the end.

## Problem

- Third parties call the hashinbox API by hand, or run a generic generator over
  `docs/openapi.json`. No generator understands our `security`:
  `"bearer": ["com.hashinbox.wallets[{id}]:read"]` is a scope **template**, filled from the
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
  - **cligen generates only the models and the operations.** Everything else in a client
    folder is hand-written: the interfaces a transport meets (`Client`, `Streamer`, `Session`),
    `ApiError`, the transports, sessions, the scope port, the manifest. No HTTP stack is
    generated.
  - No request validation (min/max, patterns): the server validates.
  - No retry, rate-limit or reconnect policy in generated code: that is the transport's.
  - Hashinbox-specific parts (scope grammar, key sessions, `?token=`) never live in cligen; they
    live in the hand-written files of `apps/server/clients/<lang>/`.

## Pieces

| Piece | Where | Written |
|---|---|---|
| OpenAPI subset parser (serde) | `packages/cligen/src/openapi.rs` | once |
| `Spec`: resolved models and operations | `packages/cligen/src/spec.rs` | once |
| One generator per language | `packages/cligen/src/{typescript,go,rust}/{mod,expr}.rs` | once per language |
| Models + operations | `apps/server/clients/<lang>/`, written by `apps/server/build.rs` | generated |
| `Client`, `Streamer`, `Session`, `ApiError`, helpers, scope port, sessions, transports, manifest | `apps/server/clients/<lang>/` | hand-written |
| Scope conformance vectors | `apps/server/clients/_golden/scope.json`, from `packages/scope` | generated (later) |

## The generator

```text
docs/openapi.json ─ openapi.rs (serde, deny_unknown_fields) ─ Spec::from_openapi ─ Spec
Spec ─ Go { package }.emit(&spec)   ─ models.go, operations.go
     ─ Typescript.emit(&spec)       ─ models.ts, operations.ts
     ─ Rust.emit(&spec)             ─ models.rs, operations.rs
```

### Parsing: our own serde subset

`openapi.rs` is the OpenAPI 3.1 subset as serde structs (`Document`, `PathItem`, `Operation`,
`Parameter`, `RequestBody`, `Response`, `MediaType`, `Schema`), every one
`deny_unknown_fields`. A feature outside the subset (`anyOf`, `not`, `const`, header or cookie
parameters, callbacks, `links`) fails to parse with serde's message and line; nothing is dropped
silently. Validation keywords (`minimum`, `pattern`, `default`, `examples`) are read and
ignored. The structs carry only generic helpers (`doc.operations()`, `doc.schema(name)`,
`op.path_params()`, `op.requirements()`, `schema.is_nullable()`, `schema.ref_name()`).

utoipa was the first choice and is dropped: utoipa 5 cannot read back what it writes (a schema
with no `type`, `{}` or `{"description": …}`; a parameter without `required`) and silently drops
keywords it does not model.

### `Spec`: resolved once, plain data

```rust
pub struct Spec { pub title: String, pub version: String, pub models: Vec<Model>,
                  pub operations: Vec<Operation>, pub error: Option<String> }
pub enum Model {
    Struct { name: String, doc: String, fields: Vec<Field> },
    Enum   { name: String, doc: String, values: Vec<String> },
    Union  { name: String, doc: String, variants: Vec<Type> },
}
pub struct Field { pub name: String, pub ty: Type, pub required: bool, pub doc: String }
pub enum Type { String, Int, Int32, Float, Bool, Any, Named(String),
                Array(Box<Type>), Map(Box<Type>), Optional(Box<Type>) }
pub struct Operation {
    pub id: String, pub tag: String, pub summary: String, pub doc: String,
    pub method: Method, pub path: String,
    pub params: Vec<Param>,                          // path, then query
    pub body: Option<Type>,                          // optional body: Optional(T)
    pub response: Option<Type>,                      // none: empty, or a stream
    pub events: Option<Vec<(String, Type)>>,         // x-events; Some([]): raw events
    pub scopes: Vec<String>,                         // templates, holes = path params
}
pub struct Param { pub name: String, pub ty: Type, pub path: bool, pub required: bool,
                   pub doc: String }
```

- `Spec::from_openapi(json)` is the only place with logic. Inline objects, enums and unions
  are hoisted into models named after where they were (`PetOwner`, `ReadHealthResponse`);
  `allOf` of objects is flattened; a schema that is only an alias (`String`) is inlined;
  `oneOf [null, X]` and `type: [T, "null"]` are `Optional(X)`. Generators need no lookups.
- `error` is the model most error bodies are (`Error`); `ApiError.from` decodes it, and keeps
  another body's text (`read_health`'s 503 `Report`) as `metadata.response`.
- Order: models by name, operations by tag then id. Two runs give the same bytes.

### Operation ids and the name-clash rule

- Every `operationId` is `<resource>_<verb>`, the resource first: `wallets_list`,
  `wallet_get`, `keys_create`, `network_endpoints_create`, `network_endpoint_test`. Verbs:
  `list`, `get`, `create`, `update`, `replace`, `delete`, and domain verbs (`stream`, `test`,
  `rotate`, `revoke`, `reconcile`, `search`, `send`, `start`, `stop`, `confirm`, `add`,
  `remove`). A collection's verbs take the plural (`wallets_create`), an item's the singular
  (`wallet_get`); the caller's own user is `own_user_*`.
- Generated code is one namespace per language, so every operation's names carry its
  resource: `NetworkEndpointsCreate`, `NetworkEndpointsCreateRequest`, `…Response`, `…Event`.
  Models keep their schema names.
- A name clash is an error in `Spec::from_openapi`, never a silent rename: two operations of
  one id, two different models of one name, or a model named like an operation's
  `Pascal(id)`, `…Request`, `…Response` or `…Event`. Fixed at the source in the server:
  request bodies named after their operation became `…Request` (`CreateKeyRequest`, …,
  `WriteSettingRequest`), and `NetworkEndpointsList` (the networks a change touched) became
  `NetworkEndpointsChanged`. Inline response schemas are hoisted as `…Output`.

### One generator per language

Each language is a struct of its options, exactly:

```rust
pub struct Go { pub package: String }        // Typescript and Rust have no options
impl Go {
    pub fn emit(&self, spec: &Spec) -> Files;  // models.go + operations.go
    fn model(&self, out: &mut String, model: &Model);
    fn operation(&self, out: &mut String, op: &Operation);
}
```

`model` and `operation` map the `Spec` onto the language's expressions and print them; the
syntax lives in the language's `expr.rs`, in the shape of the `codegen` crate (not a
dependency): small structs with public fields and a builder method or two (Go: `File`,
`Struct`/`Field` with tag or embedded, `Enum`, `Var`, `Func` with receiver, `Block`,
`Switch`; TS: `Import`, `Interface`, `TypeAlias`, `Const`, `Function`, `JsDoc`; Rust: `Use`,
`Struct`, `Enum`, `Const`, `Alias`, `Impl`, `Fn`, `Body`; PHP: `File`, `Use`, `Class`
(`Class::readonly`) with promoted `Property`s, `Method`, `Enum`, `Const`, `DocBlock`, `Body`,
`Value`), each writing itself through a
small `Formatter` that tracks indentation and implementing `Display`. Spec types and names
print through wrappers (`GoType(&ty)`, `GoName(&id).exported()`, `TsType`, `TsName`,
`RustType`, `RustName`, `PhpType`, `PhpName`), which also hold each language's reserved words.

- **Naming** is pure: `pascal`, `camel`, `snake` (heck's); each language escapes its own
  reserved words; Go upper-cases golint's initialisms (`ExternalID`).
- **Formatter-clean output:** `gofmt` (tabs, aligned fields and constants), `prettier`
  defaults, `rustfmt` defaults. Tests run the formatters' checks where installed.
- Every generated file starts with `// Code generated by cligen from hashinbox-api 0.1.0. DO
  NOT EDIT.`

### OpenAPI features in v1

| In | Out (fails to parse or resolve) |
|---|---|
| `$ref` to `#/components/schemas/*` | external `$ref`, `$ref` to parameters/responses |
| `type` arrays with `null`, `oneOf` with `null` | `anyOf`, `not`, `if/then`, `patternProperties` |
| `allOf` of objects (flattened) | `allOf` mixing non-objects |
| `oneOf` untagged | `discriminator` |
| `enum` of strings | non-string enums, `const` |
| `format` `int32`/`int64` (int width) | `format` as validation |
| `additionalProperties` (map), `propertyNames` ignored | tuple `prefixItems` |
| path and query params, scalar or enum | header/cookie params, array/object query params |
| `application/json` bodies; `text/event-stream` responses with `x-events` | multipart, form, binary |
| one `security` requirement with scope templates | several requirements, callbacks, webhooks, links |

## Alternatives considered

| Tool | What it does | Why not (alone) |
|---|---|---|
| `utoipa` (`openapi::OpenApi`) | Typed OpenAPI 3.1 model, `Deserialize` | Tried first: cannot read back schemas without `type` or parameters without `required`, and drops unknown keywords silently. Replaced by our own serde subset. |
| `codegen` (carllerche) | Rust code builders | The model for each language's `expr.rs`; not a dependency (Rust only, and each language needs its own syntax). |
| `oas3` | OpenAPI 3.1 parser | Good, but not in the offline cache, and the subset we read is a few serde structs. |
| `openapiv3` | OpenAPI 3.0 parser (progenitor's) | 3.0 only; our spec is 3.1. |
| Kiota (Microsoft) | Generates models and request builders for TS, Go, Python, C#, Java, PHP, Ruby over a pluggable `RequestAdapter` and `AuthenticationProvider` | Closest to this design, but has no scopes, `.can()`, typed SSE events or our session/JWT model; we would wrap or post-process its output and carry its runtime. |
| openapi-generator | Whole HTTP clients in 50+ languages | Generates the transport (against our interfaces-only rule); output is hard to make idiomatic. |
| progenitor (Oxide) | Rust clients | 3.0 only; generates the HTTP client too. |
| oapi-codegen | Go types and clients | Go only; could serve Go models, but not scopes or the shared shape. |
| openapi-typescript | TS types only | What the dashboard used before `@hashinbox/client`; no operations, scopes or sessions. |

What none of them does is the point of cligen: operations as functions with `.scopes` and
`.can`, generated interfaces only (no HTTP stack), one shape across languages, typed SSE events.
So cligen owns a small parser of the subset it supports, the resolving, and the emitting.

## The hand-written client

Each client has one hand-written file the generated operations call (`src/client.ts`,
`client.go`, `src/client.rs`), plus `scope.*` (the escape of a scope hole). It is the
starting point the clients grow from:

| | Go | TypeScript | Rust |
|---|---|---|---|
| Descriptor | `Operation{ID, Method, Path, Templates}` | `Operation { id, method, path, query?, templates, events? }` | `Operation { id, method, path, templates }` |
| Request | interface `Request`: `Operation()`, `Params()`, `Query()`, `Body()` | any object: its fields by name, `body` | trait `Request`: `type Response`, `operation()`, `param(name)`, `query(&mut)`, `body()` |
| Client | `Client.Do(ctx, request, &response) error` | `Client.send<T>(operation, request, signal)` | `Client::send(Http)`; `call(&client, &request)` decodes |
| Streams | `Streamer.Open(ctx, request, cursor)`, `Open[T]` → `*Stream[T]` (a handle: `Next`, `Stop`, `Cursor`) | `Streamer.open<E>(operation, request, signal)` → `EventStream<E>` (a handle: async iterable, `stop()`, `cursor`) | `Streamer::open(Http)` → events or the non-2xx `Response`; `open(&streamer, &request)` → `Events<T, S>` (a handle: `Stream`, `stop()`, `cursor()`) |
| Path, scopes | `Path(request)`, `op.Scopes(params)`, `session.Can(op, params)` | `path`, `scopes`, `can` | `request.path()`, `.scopes()`, `.can(session)` |

- The transport prefixes its base URL and adds the credential; generated code never sees a
  header or a host.
- **Errors:** one `ApiError` for any failure: a plain typed value of `kind`, `message`,
  `reason?`, `metadata`, `status`; its constructor takes those fields and parses nothing
  (defaults: `kind` `unavailable`, `status` 0, `metadata` empty). `from(x)` (Python `of`) is the
  one way to make one of anything, and every client path (send, stream open and reconnect,
  login, a key's scope read) goes through it:
  1. an `ApiError`: as is;
  2. an answer the client got (its status and read body): JSON in the error shape (`Error`: a
     known `kind`, a `message`) is that error with its status; anything else (an HTML 502, an
     empty body, other JSON, an unknown kind) is `unavailable`, message `could not parse the
     response (HTTP <status>)`, its status, `metadata.response` its first 1024 characters;
  3. an error thrown (network, timeout, abort, …): `unavailable`, status 0, its message, the
     original as its cause;
  4. anything else: `unavailable`, as a string.

  A call the session's scopes do not cover is refused locally, unsent, as the server's own
  refusal, built with the constructor: status 403, `permission_denied`, reason `missing_scope`,
  message `<operation> needs scope <scope>`, metadata `{scope, operation}`. One error type for
  local and server refusals. `Reason` names the server's reasons
  (`wrong_credentials`, `totp_required`, `session_expired`, `missing_scope`):
  TS `ApiError.from(e).reason === Reason.TotpRequired`; Go `APIErrorFrom(err).Reason ==
  ReasonTOTPRequired`; Rust `ApiError::from(e).reason.as_deref() == Some(reason::TOTP_REQUIRED)`;
  Python `ApiError.of(e).reason == Reason.TOTP_REQUIRED`; PHP `ApiError::from($e)->reason ===
  Reason::TotpRequired`.
- **Cancellation:** TS `AbortSignal`, Go `context.Context`, Rust dropping the future.

### Rust: `apps/server/clients/rust`

Crate `hashinbox-client`, its own workspace; `scope` is `packages/scope` as a path dependency
(no port). Hand-written: `client.rs` (the interface), `client/http.rs` (the default transport,
feature `reqwest`, on by default), `session.rs`, `secret.rs`, `scope.rs`. Without the feature,
the trait, the models, the operations, `Session` and `Secret` still compile.

| Item | API |
|---|---|
| Scopes | `scope::{Scope, Scopes, ParseError}` re-exported; `escape_id(id)`; `fill(template, \|name\| value)` |
| `Session` | `{ token, issuer, subject, scopes, issued_at, expires_at, amr }`; `Session::parse(jwt)` (decoded, **not verified**: HS256, `kid` = `iss`); `expired()`; `can(&OPERATION, &[("id", "…")])`; implements `client::Session` |
| `Secret` | `Secret::parse("hip/…", "his/…")` (base 58, CRC32 checked); `secret.issue(Issue::new().scope("…")?.subject("…").ttl(d))`: HS256, the `his/…` text the key, header `{typ, alg, kid}` and claims `{iss, sub?, iat, exp, scopes}` in the server's order (byte-identical to its `jsonwebtoken`); TTL 10 min unless set, 1 s to 24 h |
| `Client` | `Client::builder(url).session(s).secret(s).scopes(s).ttl(d).http(reqwest::Client).timeout(d).check_scopes(bool).build()`; implements `client::Client` and `client::Streamer`; `login(email, password, totp)`, `logout(all)`, `session()`, `stream(&request)` → `Events<T, http::Error>` |
| Errors | `ApiError { status, kind, message, reason, metadata, cause }` (`..ApiError::default()`: `Unavailable`, status 0); `ApiError::from(response)` (`From<Response>`) and `ApiError::from(error)` (`From<Error<T>>`: `Api` as is, any other with `cause`); `Error::Api(ApiError)` (an answer, or a missing scope, unsent); `Error::Decode`; `Error::Transport(http::Error)`; `error.reason()`; `reason::{WRONG_CREDENTIALS, TOTP_REQUIRED, SESSION_EXPIRED, MISSING_SCOPE}` |

- **A session is always a JWT; a secret never goes over the wire.** With `.secret(..)` the client
  signs its own sessions and sends them as the bearer; there is no `Bearer his/…` path. It keeps
  one (behind a lock) and signs a fresh one within 60 s of its expiry (half the TTL if shorter),
  or after `401 session_expired`: one retry, then the error.
- Its sessions hold the builder's `.scopes(..)`, or else the key's: read once with a 60 s session
  of no scopes, `GET /v1/session` → `key.scopes` (that route needs no scope); re-read after
  `session_expired`.
- **Clock:** the server takes `iat` at most 60 s ahead of its clock, at most 24 h of life, and
  refuses an `exp` in its past. A signer whose clock is more than 60 s fast, or slower than the TTL,
  is refused: keep it on NTP.
- `check_scopes` (on by default): a call the kept session lacks a scope for fails as the
  server's 403 `missing_scope` `Error::Api`, unsent (the trait's `missing(scopes)`; `None` when it cannot tell, before a
  key's first session).
- `stream()`: a minimal SSE reader (`event`, multi-line `data`, `id`; comments skipped) behind
  the reconnecting handle `Events` ([Streams](#streams-the-15-s-window-and-the-handle)):
  `stop()` or drop ends it, `cursor()`, `backoff(first, most)`; a reconnect sets the query's
  `cursor`, `Streamer::sleep` waits the backoff.
- Tests (`cargo test --offline` in the folder): the `_golden/` scope and session cases (the
  signing byte-identical, a server-made token parsed), the client against a loopback server
  (path, query, body, bearer, error mapping, a local missing scope, signing and re-signing, login,
  SSE), a live smoke test when `HASHINBOX_URL` is set; `examples/backend.rs` creates a wallet
  with a secret and issues a 15-minute browser token.

### Go: `apps/server/clients/go`

Package `hashinbox`, stdlib only (`net/http`, `crypto/hmac`, `crypto/sha256`, `encoding/json`,
`hash/crc32`). Hand-written: `client.go` (the interfaces the generated code uses, and
`HTTPClient`), `scope.go` (a port of `packages/scope`), `session.go`, `secret.go`. The
generated operations take the `Client` interface, so the default transport is `*HTTPClient`.

| Item | API |
|---|---|
| Scopes | `ParseScope(text)`, `scope.Covers(other)`, `.String()`; `Scopes` (`[]Scope`): `ParseScopes(packed)`, `MustScopes(texts...)`, `.Covers(scope)`, `.FirstUncovered(others)`, `.Pack()`; `EscapeID(id)`, `Fill(template, params)` (one pass, holes escaped) |
| `Session` | `{Token, Issuer, Subject, Scopes, IssuedAt, ExpiresAt, Amr}`; `ParseSession(jwt)` (decoded, **not verified**); `.Expired(now)`; `.Can(op, params)` |
| `Secret` | `ParseSecret("hip/…", "his/…")` (base 58, CRC32 checked); `secret.Issue(WithScopes(s), WithSubject(sub), WithTTL(d))`: HS256, the `his/…` text the key, header `{typ, alg, kid}` and claims `{iss, sub?, iat, exp, scopes}` in the server's order, no HTML escaping (byte-identical to its `jsonwebtoken`); TTL 10 min unless set, 1 s to `MaxTTL` (24 h); `String()` prints the id only |
| `HTTPClient` | `NewClient(baseURL, WithSession(s) \| WithSecret(s), WithSecretScopes(s), WithSessionTTL(d), WithHTTPClient(c), WithTimeout(d), WithUserAgent(ua), WithoutScopeCheck())`; `Do` (`Client`), `Open` (`Streamer`); `Login(ctx, LoginRequest{Email, Password \| Passphrase, TOTP})`, `Logout(ctx, all)`, `Session()` |
| Errors | `*APIError{Status, Kind, Message, Reason, Metadata, Cause}`, a struct literal; `APIErrorFrom(x any)` (never nil; the `*APIError` an error wraps, as is); `ReasonWrongCredentials`, `ReasonTOTPRequired`, `ReasonSessionExpired`, `ReasonMissingScope`; match on `APIErrorFrom(err).Reason`; a local missing scope is the server's 403 `*APIError`, unsent |

- **A session is always a JWT; a secret never goes over the wire.** `WithSecret` makes the
  client sign its own sessions (`secret.Issue`) and send them; there is no `Bearer his/…` path.
  It keeps one behind a mutex, signs a fresh one within 60 s of its expiry, and after a `401
  session_expired` retries once, then returns the error.
- Its sessions hold `WithSecretScopes(..)`, or else the key's: read once with a 1-minute session
  of no scopes, `GET /v1/session` → `key.scopes` (that route needs no scope); re-read after
  `session_expired`. TTL 10 min, `WithSessionTTL(d)` (more than 60 s, at most 24 h).
- **Clock:** the server takes `iat` at most 60 s ahead of its clock, at most 24 h of life, and
  refuses an `exp` in its past: keep the signer on NTP.
- `Do` fills the path (`url.PathEscape`), the query and the JSON body, adds `Bearer <jwt>`,
  bounds the call by `WithTimeout`, and decodes a 2xx body (a response of one `Value` field, a
  route of no model, decodes into it). `Login` sends no `Authorization` and replaces the
  session (and any secret); `Logout(ctx, false)` only forgets, `true` first calls
  `DELETE /v1/session?all=true`.
- `Open`: a minimal SSE reader (`event`, multi-line `data`, `id`; comments skipped; `message`
  by default), setting the query's `cursor` when given one; the generated `Open[T]` wraps it as the
  reconnecting handle `*Stream[T]` ([Streams](#streams-the-15-s-window-and-the-handle)):
  `Next()` (`io.EOF` at the end), `Stop()` (or ctx cancel; `Close` the same), `Cursor()`;
  `WithStreamBackoff(first, most)`. The first connect is in `Open`, its error returned there.
- Tests (`go test ./...` in the folder): every `_golden/scope.json` case; `_golden/session.json`
  signed byte for byte and a user token parsed; `HTTPClient` against `httptest` (path, query,
  body, bearer, error mapping, a local missing scope, signing, caching, re-signing, retry,
  login, logout, SSE cancelled by ctx); a live smoke test when `HASHINBOX_URL` is set (and
  `HASHINBOX_KEY_ID`, `HASHINBOX_SECRET` for a key). `examples/backend/main.go` creates a
  wallet with a key and issues a 15-minute token that reads only it.

## The generated operation

### Files

- Exactly two per language: `models` (every model) and `operations` (every operation, by tag
  then id). No per-tag files, so no hand-kept list of them: `index.ts`, `lib.rs` re-export
  `client`, `models`, `operations`, `scope`.

### Go: `wallet_get`

```go
// walletGet is GET /v1/wallets/{id}.
var walletGet = Operation{
	ID:        "wallet_get",
	Method:    "GET",
	Path:      "/v1/wallets/{id}",
	Templates: []string{"com.hashinbox.wallets[{id}]:read"},
}

// WalletGetRequest is WalletGet's input: its path and query parameters, and its body.
type WalletGetRequest struct {
	// `{chain}.{network}.{address}`, as a create call returned it.
	ID string `json:"-"`
}

func (r WalletGetRequest) Operation() Operation       { … } // walletGet
func (r WalletGetRequest) Params() map[string]string { … } // {"id": r.ID}
func (r WalletGetRequest) Query() url.Values          { … } // nil
func (r WalletGetRequest) Body() ([]byte, error)      { … } // nil, nil

// WalletGetResponse is WalletGet's output.
type WalletGetResponse struct {
	Wallet
}

// WalletGet reads one wallet's public metadata.
func WalletGet(ctx context.Context, client Client, request WalletGetRequest) (WalletGetResponse, error) {
	var response WalletGetResponse
	err := client.Do(ctx, request, &response)
	return response, err
}
```

- A body is embedded in the request (`NetworkEndpointsCreateRequest { Chain; Network; RpcNodes }`,
  `Body()` marshals `r.RpcNodes`); a response model is embedded in the response; another type
  is its `Value` field; none, an empty struct. A stream returns `*Stream[WalletGetEvent]` from
  `Open`, or `*Stream[RawEvent]` without `x-events`.

### TypeScript: `wallet_get`

```ts
/** `GET /v1/wallets/{id}`. */
export const walletGetOperation: Operation = {
  id: "wallet_get",
  method: "GET",
  path: "/v1/wallets/{id}",
  templates: ["com.hashinbox.wallets[{id}]:read"],
};

/** The input of `walletGet`. */
export interface WalletGetRequest {
  /** `{chain}.{network}.{address}`, as a create call returned it. */
  id: string;
}

/** The output of `walletGet`. */
export type WalletGetResponse = Wallet;

/** Read one wallet's public metadata. `GET /v1/wallets/{id}` */
export function walletGet(
  client: Client,
  request: WalletGetRequest,
  signal?: AbortSignal,
): Promise<WalletGetResponse> {
  return client.send<WalletGetResponse>(walletGetOperation, request, signal);
}
```

- The descriptor names its query fields (`query: ["cursor", "limit"]`) and a stream's events;
  a body is `request.body`; no fields is `Record<string, never>` with a `{}` default.

### Rust: `wallet_get`

```rust
/// `GET /v1/wallets/{id}`.
pub const WALLET_GET: Operation = Operation {
    id: "wallet_get",
    method: Method::Get,
    path: "/v1/wallets/{id}",
    templates: &["com.hashinbox.wallets[{id}]:read"],
};

/// The input of [`wallet_get`].
#[derive(Clone, Debug)]
pub struct WalletGetRequest {
    /// `{chain}.{network}.{address}`, as a create call returned it.
    pub id: String,
}

/// The output of [`wallet_get`].
pub type WalletGetResponse = Wallet;

impl Request for WalletGetRequest {
    type Response = WalletGetResponse;

    fn operation(&self) -> &'static Operation {
        &WALLET_GET
    }

    fn param(&self, name: &str) -> Option<String> {
        match name {
            "id" => Some(self.id.to_string()),
            _ => None,
        }
    }
}

/// Read one wallet's public metadata: `GET /v1/wallets/{id}`.
pub async fn wallet_get<C: Client>(
    client: &C,
    request: WalletGetRequest,
) -> Result<WalletGetResponse, Error<C::Error>> {
    call(client, &request).await
}
```

- `query` and `body` are generated only when the operation has them; optional query
  parameters are `Option<T>` fields, and a request of only those derives `Default`.

### Python: `wallet_get`

`cligen::python::Python.emit(&spec)` (no options; `src/python/{mod,expr}.rs`, expressions
`Module`, `Import`, `Class` (`Class::dataclass`), `Field`, `Enum`, `TypeAlias`, `Def`,
`Docstring`, `Value`; `PyType`, `PyName`) writes `models.py` and `operations.py` into
`apps/server/clients/python/hashinbox/`. Python 3.11+, typed (`str | None`, `list[X]`),
`ruff format`/`black` clean (88 columns; a bracket that does not fit is one item per line with a
trailing comma), `mypy --strict` clean. The marker is a `#` comment.

```python
# `GET /v1/wallets/{id}`.
WALLET_GET = Operation(
    id="wallet_get",
    method="GET",
    path="/v1/wallets/{id}",
    templates=("com.hashinbox.wallets[{id}]:read",),
)


@dataclass(frozen=True, kw_only=True)
class WalletGetRequest(Request):
    """The input of `wallet_get`."""

    # `{chain}.{network}.{address}`, as a create call returned it.
    id: str

    def operation(self) -> Operation:
        return WALLET_GET

    def params(self) -> dict[str, str]:
        return {"id": self.id}


# The output of `wallet_get`.
WalletGetResponse: TypeAlias = Wallet


async def wallet_get(client: Client, request: WalletGetRequest) -> WalletGetResponse:
    """Read one wallet's public metadata: `GET /v1/wallets/{id}`."""
    return Wallet.from_json(await client.send(request))
```

- **Models** are `@dataclass(frozen=True, kw_only=True)` with a generated `from_json(data)`
  classmethod and `to_json()` (an optional field is left out when `None`). Enums are
  `StrEnum` (an unknown value fails to decode). A union is a quoted alias
  (`Listed: TypeAlias = "Setting | SecretSetting"`) and a `listed_from_json` that takes the first
  variant that decodes.
- **Requests** subclass the hand-written `Request` protocol and override only what they have:
  `operation()`, `params()` (path), `query()` (pairs, `None` skipped), `content()` (the body's
  JSON text; the field is `body`, so the method cannot be). Fields named like a method get a
  `_`, as do keywords. A request of only optional fields is the call's default.
- **Hand-written** (`client.py`): `Operation`, `Request`, `Granted`, `Streamer`
  (`open(request) -> Stream[RawEvent]`, the reconnecting handle), `Event[T]`, `path`, `scopes`, `can`, the JSON
  helpers the generated code calls (`optional`, `array`, `mapping`, `first`, `encode`, `dump`,
  `data`, `text`), and `Client`: the generated ops' `client: Client` is the default httpx
  client itself (another transport goes in as `http=httpx.AsyncClient(transport=…)`). See
  "Python: `hashinbox` (built)".
- **Streams:** `x-events` gives `PetEventsStreamEvent: TypeAlias = "Event[Bark] | Event[Missed]"`
  and `streamer.open(request).map(decode)`, a `Stream` decoding each named event (others
  skipped); without `x-events` the call returns the `Streamer`'s `Stream[RawEvent]`.
- cligen's tests run `python3 -m py_compile`, `ruff format --check` (else `black --check`)
  and `mypy --strict` (else `pyright`) when installed.

### PHP: `wallet_get`

`cligen::php::Php { namespace: "Hashinbox".into() }.emit(&spec)` (`src/php/{mod,expr}.rs`)
writes `Models.php` and `Operations.php` into `apps/server/clients/php/src/`. PHP 8.2+,
`declare(strict_types=1)`, PER-CS 2.0 (120 columns; a list that does not fit is one item per
line with a trailing comma; `php-cs-fixer --rules=@PER-CS2.0` clean), PHPStan level max clean.
The marker is a `//` line after `<?php` (`build.rs` looks past `<?php` for it).

```php
/**
 * `GET /v1/wallets/{id}`.
 */
const WALLET_GET = new Operation(
    id: 'wallet_get',
    method: 'GET',
    path: '/v1/wallets/{id}',
    templates: ['com.hashinbox.wallets[{id}]:read'],
);

/**
 * The input of `walletGet`.
 */
final readonly class WalletGetRequest implements Request
{
    /**
     * @param string $id `{chain}.{network}.{address}`, as a create call returned it.
     */
    public function __construct(public string $id) {}

    public function operation(): Operation
    {
        return WALLET_GET;
    }

    public function params(): array
    {
        return ['id' => $this->id];
    }

    public function query(): array
    {
        return [];
    }

    public function content(): ?string
    {
        return null;
    }
}

/**
 * Read one wallet's public metadata: `GET /v1/wallets/{id}`.
 */
function walletGet(Client $client, WalletGetRequest $request): Wallet
{
    return Wallet::fromJson(Json::object($client->send($request)));
}
```

- **Files:** two, each of many declarations, so PSR-4 (one class per file) cannot load them:
  composer's `"files"` autoload includes both (option b); the hand-written classes load by
  PSR-4 (`Hashinbox\` → `src/`). Functions and `const` (with `new`, PHP 8.1+) need `"files"`
  anyway. PHPStan analyses them from `src/` like any other file.
- **Models** are `final readonly class`es with promoted properties (camelCase; required ones
  first, optional ones `= null`), `fromJson(array $data): self` (named arguments; every value
  checked by the hand-written `Json::string/int/float/bool/object/list/map/field`, which throw
  `UnexpectedValueException`) and `toJson(): array` (an optional field left out when null;
  an enum as its value; a nested model or a map as `stdClass`, so an empty one stays `{}`).
  PHPDoc carries what native types cannot: `@param list<Pet>|null $items`,
  `array<string, X>`. Enums are `enum Kind: string` (cases PascalCase; an unknown value fails to
  decode). A union is its native union type (`Setting|SecretSetting`) wherever it is used, and
  `listedFromJson(mixed $data)` takes the first variant that decodes (`Json::first`).
- **Requests** implement the hand-written `Request` interface, all four methods always:
  `operation()`, `params()` (path), `query()` (pairs, null skipped; bool `'true'`/`'false'`,
  enum its value), `content()` (`Json::dump` of the body; `string` when the body is required).
  A field named `body` becomes `body_`; reserved words get a `_` (`List_`, `list_`).
- **Calls** are namespaced functions; the response is the return type (PHP has no type
  aliases). A request of only optional fields defaults to `new XRequest()`. A stream returns
  `Stream<RawEvent>`, or with `x-events` `$client->open($request, $decode)`, a `Stream`
  decoding each named event into `Event<Bark>|Event<Missed>` (others skipped).
- cligen's tests run `php -l` and `phpstan analyse --level=max src` on a scratch copy, and
  `php-cs-fixer fix --dry-run --rules=@PER-CS2.0` on the golden and real output, each when
  installed (PHP is not here: in Docker, `php:8.3-cli`).

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
  `Scope::covers`, `Scopes::unpack`, `pack`, `compress`, `expand` and `escape` (about 150 lines
  each). Rust uses `packages/scope` itself, no port.
- **Dependencies:** the document's top-level `x-hashinbox-depends` (source → targets, design
  096) is read by cligen into `Spec::depends` and generated as `DEPENDS` (Go: `Depends`) in
  each operations file. `Session.parse` expands its scopes with it, as the server's middleware
  does, so `can()` answers what the server will.
- **Conformance vectors** keep the ports honest: a test in `packages/scope`
  (`cargo test -p scope vectors`, `BLESS=1` to rewrite) writes
  `apps/server/clients/_golden/scope.json` from the crate's own answers:

```json
{ "parse":   [{ "text": "com.hashinbox.wallets[a,b%5D%20c%25d]", "display": "…" }],
  "refuse":  ["", ":read", "wallets:write", "wallets[]", "wallets[%zz]"],
  "covers":  [{ "ours": "a.wallets[x]", "theirs": "a.wallets[x].tags:read", "covers": true }],
  "sets":    [{ "scopes": "a.wallets[x]:read a.markets:read", "compressed": "…",
                "packed": { "a": ["markets:read", "wallets[x]:read"] } }],
  "first_uncovered": [{ "granted": "…", "asked": "…", "first_uncovered": "…" }],
  "expand":  { "depends": { "a.triggers": ["a.wallets"] },
               "cases": [{ "scopes": "a.triggers:read", "expanded": "a.triggers:read a.wallets:read" }] },
  "escape":  [{ "id": "a,b] c%d", "text": "a,b%5D%20c%25d" }] }
```

- The cases are the crate's existing test cases (`coverage`, `bad_text_is_refused`,
  `ids_are_escaped`, `compress_drops_covered`) moved into one table both sides read. CI fails if
  the file is stale; each language's test suite runs every vector.

## Sessions and keys per language

Hand-written, in `apps/server/clients/<lang>/session.*`, the same three things everywhere:

A **Session is always a JWT**; a key's secret never goes over the wire.

| | Does | Scopes from |
|---|---|---|
| `Session.parse(jwt)` | holds a JWT someone issued (a login, or a token your backend signed); sent as `Bearer <jwt>` | the JWT's `scopes` claim, unpacked and expanded with `DEPENDS`; **not verified** (the client cannot; the server does) |
| `Secret.issue({scopes?, subject?, ttl?})` | signs an HS256 JWT (096) with the `his/…` text and answers a `Session` | the scopes it signed |
| the client with a `secret` | signs its own sessions (10 min default), caches them, re-signs within 60 s of expiry or once after `session_expired`; never sends `his/…` | the scopes passed, else the key's own (read once with `GET /v1/session` on a scopeless signed session) |

The machine's clock must be roughly right: the server accepts `iat` up to 60 s ahead, and at most
24 h of life.

Signing, per 096: header `{alg: HS256, typ: JWT, kid: hip/…}`; claims `iss` (= `kid`), `sub?`,
`iat`, `exp` (≤ 24 h), `scopes` packed (`Scopes::pack`); HMAC key = the whole `his/…` text.
No JWT library: base64url + HMAC-SHA256 from the platform.

| Language | HMAC from | Signature |
|---|---|---|
| TS | WebCrypto `crypto.subtle` (browsers, Node ≥ 20, Deno, Bun) | `await secret.issue({ scopes, subject?, ttl? })` → `Session`: async, WebCrypto is |
| Go | `crypto/hmac`, `crypto/sha256` | `secret.Issue(WithScopes(..), WithSubject(..), WithTTL(..)) (Session, error)` |
| Rust | `hmac` + `sha2` | `secret.issue(Issue::new().scope(..)?.subject(..).ttl(..)) -> Result<Session, Invalid>` |
| Python | `hmac`, `hashlib` | `secret.issue(scopes=None, subject=None, ttl=600) -> Session` |
| PHP | `hash_hmac('sha256', …)` (ext-hash) | `$secret->issue(?Scopes $scopes = null, ?string $subject = null, int $ttl = 600): Session` |

- One TS package for browser and Node: WebCrypto removes the need for a separate `node` client.
  A browser never holds `his/…`; it gets a narrow JWT from its own server (096 need 3).
- `session.scopes()` returns the unpacked list; `session.expiresAt` from `exp`, so a transport
  can refresh before it lapses.
- **Streams and `?token=`:** the default `Streamer` is fetch-based and sends the header. A
  user who wants `EventSource` passes `{ tokenInQuery: true }`; only the transport appends
  `?token=<jwt>` (never a `his/…` secret), and only for GET (096 open question 7).

### TypeScript: `@hashinbox/client` (built)

One universal package: browser and Node ≥ 20, ESM, web standards only (`fetch`,
`crypto.subtle`, `ReadableStream`, `AbortSignal`), no runtime dependencies. Hand-written
`src/{client,scope,session,secret,index}.ts`; `index.ts` re-exports them and the generated files.

| Piece | API |
|---|---|
| `scope.ts` | `Scope.parse`, `.covers`, `.toString`; `Scopes.parse(text \| packed)`, `.covers`, `.firstUncovered`, `.compress`, `.pack`; `escapeId`, `fill(template, params)`. A port of `packages/scope`: same grammar, `%XX` escapes, Rust's `Ord` for `compress` |
| `session.ts` | `Session.parse(jwt)` (decoded, not verified): `token`, `issuer`, `subject`, `scopes`, `issuedAt`, `expiresAt` (Unix s), `amr`; `.expired(now?)`, `.can(operation, params)` |
| `secret.ts` | `Secret.parse(id, value)` checks `his/…`'s base-58 CRC32; `await secret.issue({ scopes, subject?, ttl = 600 })`: HS256 over the claims as the server writes them, byte for byte |
| `client.ts` | `new HashinboxClient(baseUrl = '', { session?, secret?, scopes?, ttl?, fetch?, timeout?, onSession?, onUnauthorized?, checkScopes = true })`: `Client` + `Streamer` for every generated op; `login`, `logout({ all })`, `session` get/set, `stream()` |

- **A session is always a JWT; a secret is never sent.** With `secret`, the client signs its
  own sessions: `scopes`, or the key's own (read once from `GET /v1/session` with a scopeless
  session), lasting `ttl` (600 s). It keeps one, signs anew within 60 s of its `exp`, and once
  on a 401 `session_expired` (a second one surfaces). The machine's clock must be roughly right:
  the server takes an `iat` at most 60 s ahead and at most 24 h of life.
- `send()` fills the path (URI-encoded), the query and the JSON body, adds `Bearer <jwt>`, and
  joins the caller's `AbortSignal` with `timeout`. A non-2xx answer is an `ApiError { status,
  kind, message, reason, metadata }` (`new ApiError({ kind?, message, reason?, metadata?,
  status? })`); `ApiError.from(x)` makes one of anything, and `Reason` names the reasons.
- A 401 calls `onUnauthorized(error)` when a session was sent and its reason is not wrong input
  (`wrong_credentials`, `totp_required`: the 2FA forms' and the login's).
- `checkScopes`: a call the session's scopes do not cover throws the server's 403
  `missing_scope` `ApiError` before it is sent; an op with no templates is never checked.
- `login({ email, password | passphrase, totp? })` posts `session_create` with no
  `Authorization`, keeps the session and calls `onSession`; its errors keep their `reason`
  (`totp_required`: ask the code; `wrong_credentials`).
- `stream()` (and `open()`, the generated `Streamer`): fetch and a `ReadableStream` SSE parser,
  `RawEvent`s, or `{ event, data, id }` decoded once the descriptor carries `x-events`, as an
  `EventStream` handle ([Streams](#streams-the-15-s-window-and-the-handle)): `for await` it;
  `stop()`, the consumer's `break` or the signal end it; `cursor`; `{ backoff: { initial,
  max } }` on the client. No `?token=`.
- Tests (`npm test`, `node --test` with a resolve hook, no dependencies): every
  `_golden/scope.json` case; `_golden/session.json` (from `Session::to_jwt` in the server,
  `BLESS=1` rewrites it) signed byte for byte and a user token parsed; the client against a
  fake `fetch`; a live smoke test when `HASHINBOX_URL` is set. `examples/` (backend, browser,
  dashboard) are type-checked only.

### Python: `hashinbox` (built)

Python 3.11+, async, `mypy --strict` and `ruff` clean; one runtime dependency, `httpx`.
Hand-written `hashinbox/{client,scope,session,secret,__init__}.py`; `__init__` re-exports them
and the generated files.

| Piece | API |
|---|---|
| `scope.py` | `Scope.parse`, `.covers`, `str()`; `Scopes.parse(text \| packed)`, `.covers(scope \| text)`, `.first_uncovered`, `.compress`, `.pack`; `Segment`, `ScopeError`, `escape_id`, `fill(template, params, escape=escape_id)`. A port of `packages/scope`: its grammar (`%+5` too), `%XX` escapes, Rust's `Ord` and `split_whitespace` |
| `session.py` | `Session.parse(jwt)` (decoded, not verified), a frozen dataclass: `token`, `issuer`, `subject`, `scopes`, `issued_at`, `expires_at` (Unix s), `amr`; `.expired(now=None)`, `.can(operation, params)` |
| `secret.py` | `Secret.parse(id, value)` checks `his/…`'s base-58 CRC32 (`zlib.crc32`); `secret.issue(scopes=None, subject=None, ttl=600, issued_at=None)`: HS256 with `hmac`/`hashlib`, the claims as serde writes them, byte for byte; its `repr` hides the secret |
| `client.py` | `Client(base_url, *, session=None, secret=None, scopes=None, ttl=600, http=None, timeout=30.0, check_scopes=True)`, `async with`; `send`, `open`, `stream`, `login(email, password=None, passphrase=None, totp=None)`, `logout(all=False)`, `session` get/set; `ApiError(message, *, kind=UNAVAILABLE, reason=None, metadata=None, status=0)`, `ApiError.of(x)` (anything as one, an `httpx.Response` parsed; `from` is reserved), `Reason` |

- **A session is always a JWT; a secret is never sent.** With `secret=`, the client signs its
  own sessions (`scopes`, else the key's own, read once from `GET /v1/session` with a scopeless
  session), keeps one, signs anew within 60 s of its `exp` and once after a 401
  `session_expired`. The machine's clock must be within 60 s of the server's.
- `send()` fills the path (each parameter URL-escaped), the query and the JSON body, adds
  `Bearer <jwt>`, and decodes the 2xx JSON; `check_scopes` raises the server's 403
  `missing_scope` `ApiError` before sending.
- `stream()` yields `Event(event, data, id)`, the data decoded from JSON; `open()` (the
  generated ops' `Streamer`) yields `RawEvent`s. Both return the reconnecting handle
  `Stream[T]` ([Streams](#streams-the-15-s-window-and-the-handle)): `async for` it; `await
  handle.stop()` or `async with handle:` ends it; `cursor`; `Client(…, backoff=0.5,
  max_backoff=8.0)`; `map(decode)` for typed ops. A stream's reads have no timeout.
- `login` sends no `Authorization`; its `ApiError.reason` is `totp_required` (ask the code,
  call again with `totp`) or `wrong_credentials`. `logout()` forgets the session; `all=True`
  first calls `DELETE /v1/session?all=true`.
- Tests (`uvx --with httpx --with pytest-asyncio --with-editable . pytest`): every
  `_golden/scope.json` case, `_golden/session.json` signed byte for byte and its user token
  parsed, the client over `httpx.MockTransport`, a live smoke test when `HASHINBOX_URL` is set.
  `examples/backend.py`: create a wallet with a key, issue a 15-minute token for it.
- `SessionRead.kind` is generated as the error `Kind` (the server names two enums `Kind`), so
  `session_get` cannot decode a session read; the client reads `GET /v1/session` raw.

### PHP: `hashinbox/client` (built)

PHP 8.2+, `declare(strict_types=1)`, PHPStan level max clean; runtime needs only `ext-curl`,
`ext-json`, `ext-hash`. Composer package `hashinbox/client`, PSR-4 `Hashinbox\` → `src/`, the
generated files through `"files"`. Hand-written, one class per file: `Scope`, `Segment`,
`ScopeError`, `Scopes`, `Session`, `Secret`, `Client`, `ApiError`, `Operation`,
`Request`, `Json`, `Event`, `RawEvent`, and the internal `Transport`, `Reply`, `CurlTransport`.

| Piece | API |
|---|---|
| `Scope`, `Scopes` | `Scope::parse`, `->covers`, `(string)`; `Scopes::parse(text \| packed)`, `->covers(scope \| text)`, `->firstUncovered`, `->compress`, `->pack`; `Scope::escapeId(id)`, `Scope::fill(template, params, ?escape)`. A port of `packages/scope`: its grammar (`%+5` too), `%XX` escapes, Rust's `Ord` (bytes, `strcmp`) and `split_whitespace` |
| `Session` | `Session::parse($jwt)` (decoded, not verified), a readonly class: `token`, `issuer`, `subject`, `scopes`, `issuedAt`, `expiresAt` (Unix s), `amr` (`list<Amr>`); `->expired(?int $now)`, `->can(Operation, array $params)` |
| `Secret` | `Secret::parse($id, $value)` checks `his/…`'s base-58 CRC32 (`hash('crc32b')`); `->issue(?Scopes $scopes = null, ?string $subject = null, int $ttl = 600, ?int $issuedAt = null)`: HS256 with `hash_hmac`, the `his/…` text the key, header and claims in the server's byte order (`json_encode` unescaped slashes and Unicode, as serde writes them); `print_r`/`var_dump` show the id only |
| `Client` | `new Client(string $baseUrl, ?Session $session = null, ?Secret $secret = null, ?Scopes $scopes = null, int $ttl = 600, float $timeout = 30.0, bool $checkScopes = true, float $backoff = 0.5, float $backoffMax = 8.0)`; `send(Request): mixed`, `open(Request, ?Closure $decode): Stream<RawEvent>`, `stream(Request): Stream<Event<mixed>>`, `login($email, $password \| $passphrase, $totp): IssuedSession`, `logout(bool $all = false)`, `session()`, `setSession()` |
| Errors | `new ApiError($message, kind: Kind::Unavailable, reason: null, metadata: [], status: 0)` (`getMessage()`, `kind`, `reason`, `metadata`, `status`); `ApiError::from($x)` (anything as one: a `Reply` parsed, a `Throwable` as `previous`); `Reason` constants; a missing scope is the server's 403 `missing_scope` `ApiError`, unsent; a transport failure is a `RuntimeException` (`curl: …`) |

- **A session is always a JWT; a secret is never sent.** With `$secret`, the client signs its
  own sessions (`$scopes`, else the key's own: read once with a 60 s scopeless session from
  `GET /v1/session`, decoded as the generated `SessionRead`, `key.scopes`), keeps one, signs anew
  within 60 s of its `exp` and once after a 401 `session_expired`. The machine's clock must be
  within 60 s of the server's (NTP).
- `send()` fills the path (each parameter `rawurlencode`d), the query and the JSON body, adds
  `Bearer <jwt>`, bounds the call by `$timeout`, and decodes the 2xx JSON (objects as arrays;
  null when empty). `$checkScopes` throws the server's 403
  `missing_scope` `ApiError` before sending.
- `open()` and `stream()` return the reconnecting handle `Stream`
  ([Streams](#streams-the-15-s-window-and-the-handle)): `foreach` it; PHP blocks, so
  `$handle->stop()` (called from the loop body) is checked after each event and before each
  reconnect; `cursor()`; `new Client(…, backoff: 0.5, backoffMax: 8.0)`. Like a
  generator, the request goes out on the first iteration. The
  default `CurlTransport` drives a `curl_multi` handle, its write callback buffering chunks the
  generator hands out as they arrive (a stream's reads have no timeout; `$timeout` bounds
  connecting). SSE: `event`, multi-line `data`, `id`, comments skipped, `\n` or `\r\n` lines.
- `login` sends no `Authorization`; its `ApiError->reason` is `totp_required` (ask the code,
  call again with `$totp`) or `wrong_credentials`. `logout()` forgets the session; `true` first
  calls `DELETE /v1/session?all=true`.
- `Client` takes an internal `Transport` as its last argument (`@internal`): `CurlTransport` by
  default, `tests/FakeTransport` in tests.
- Tests (PHPUnit in Docker: `composer install`, then `vendor/bin/phpunit` in `php:8.3-cli` and
  `php:8.2-cli`): every `_golden/scope.json` case, `_golden/session.json` signed byte for byte
  and its user token parsed, the client over the fake transport (path, query, body, bearer,
  error mapping, a local missing scope, signing, re-signing, login, logout, SSE), a live test when
  `HASHINBOX_URL` is set (and `HASHINBOX_KEY_ID`, `HASHINBOX_KEY_SECRET`).
  `examples/backend.php`: create a wallet with a key, issue a 15-minute token for it. Checks:
  `php -l` on every file, `vendor/bin/phpstan analyse --level=max src tests`.

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
- 097's `/v1/wallets/{id}/events/stream`: one entry per kind, each the envelope.
- Written by a `utoipa::Modify` beside `Bearer` in `api.rs`, from a `const EVENTS` next to each
  stream handler, so the names sit with the code that sends them.
- OpenAPI 3.2's `itemSchema` covers the same; cligen reads it too once utoipa emits 3.2.
- No `x-events`: cligen still emits the op, yielding `RawEvent`s.

## Streams: the 15 s window and the handle

- **Server.** Every `text/event-stream` answer goes through one generic helper,
  `http_server::sse::windowed(events, window)`: it lasts the window (`http_server::sse::
  SSE_WINDOW`, 15 s; the server's `AppState.stream_window`, shorter in tests), kept alive by
  comments, then sends `event: close` (data `{}`, `retry: 0`) and ends. A stream that ends
  before sends no `close`: it is done. Each reconnect is a new request, its
  token checked again (401 when it no longer holds).
- **Resuming.** Each event carries an `id:`; the reader reconnects with the last as
  the query parameter `cursor` (each stream route has it; no `Last-Event-ID` header).
  - `logs_stream`: the id is the entry's own (given as it is kept, Unix µs or one more than
    the last, the stored row's too). The feed holds its newest 1 000; a resume sends those
    after the id, and the stored ones before them when the feed no longer holds every one
    (a restart), then the live ones: none missed, none twice.
- **Clients: a handle, not a one-shot stream.** Every language's stream helper and generated
  stream op return a handle that:
  - reconnects at once on `close` (not handed to the caller), the last id as `cursor` (the
    caller's own `cursor` kept on the first connect);
  - after a failed connect (a network error, a cut body, a 5xx or 429) waits a backoff
    (500 ms doubling to 8 s, reset after a 2xx) and tries again;
  - ends on an answer without `close`, or with the language's error on another status;
  - ends for good on stop: TS `stream.stop()` (or `break`, the signal), Go `Stop()` or ctx
    cancel, Rust `stop()` or drop, Python `await handle.stop()` / `async with`, PHP
    `$handle->stop()` (checked between events and windows);
  - exposes the last event id (`cursor`). Without a stop it reconnects every window, forever.
- **Dashboard.** The logs drawer and page (`api/logs.ts`) read the TS handle; closing the
  drawer or leaving the page calls `stop()`.

## Regenerating: `apps/server/build.rs`

There is no `clients` binary. `apps/server/build.rs` (which already embeds the UI) has `cligen`
as a `[build-dependencies]` entry:

- it reads the committed `docs/openapi.json` (`cargo:rerun-if-changed`; CI already regenerates
  it from `hashinbox --openapi` and diffs it), since a build script cannot call the crate's
  own `api::openapi()`;
- it writes each language's `models` and `operations` into `clients/typescript/src/`,
  `clients/go/` (package `hashinbox`) and `clients/rust/src/`, each file only when its text
  changed, so builds do not churn mtimes;
- it deletes an older generated file (one starting with the marker) it no longer writes, and
  never touches any other file;
- a document cligen refuses (a clash) is a build warning, the clients left as they were: the
  server still builds, so `--openapi` can regenerate the document.

## Layout of `apps/server/clients/`

```text
apps/server/clients/
  typescript/                         npm: @hashinbox/client
    package.json  tsconfig.json  src/{client,scope,session,secret,index}.ts   (hand-written)
    tests/  examples/                                                         (hand-written)
    src/models.ts  src/operations.ts                                          (generated)
  go/                                 package hashinbox; module path: open question 1
    go.mod  {client,scope,session,secret}.go                                  (hand-written)
    *_test.go  examples/backend/main.go                                       (hand-written)
    models.go  operations.go                                                  (generated)
  rust/                               crate hashinbox-client; its own workspace
    Cargo.toml  src/{lib,client,client/http,session,secret,scope}.rs          (hand-written)
    tests/{vectors,client,live}.rs  examples/backend.rs                       (hand-written)
    src/models.rs  src/operations.rs                                          (generated)
  python/                             PyPI: hashinbox
    pyproject.toml  hashinbox/{__init__,client,scope,session,secret}.py       (hand-written)
    tests/  examples/backend.py                                               (hand-written)
    hashinbox/models.py  hashinbox/operations.py                              (generated)
  php/                                composer: hashinbox/client
    composer.json  phpunit.xml.dist  src/{Client,Scope,Scopes,Session,Secret,…}.php (hand-written)
    tests/  examples/backend.php                                              (hand-written)
    src/Models.php  src/Operations.php                                        (generated)
  _golden/scope.json                  packages/scope's tests/vectors.rs (BLESS=1 rewrites)
  _golden/session.json                the server's session.rs test (BLESS=1 rewrites)
```

- cligen's tests type-check these folders with fresh output on a scratch copy: `tsc -p .`,
  `cargo check --offline`, and `go vet` when Go is installed.
- **Versioning:** a client's version is the server's (`workspace.package.version`, `info.version`);
  released on the same `v*` tag. A breaking spec change is a breaking client change.

## The dashboard (`apps/server/ui`)

| Option | Verdict |
|---|---|
| **Move to `@hashinbox/client` (path dependency `file:../clients/typescript`)** | **Done.** One TS client in the repo; `session.can(networksListOperation)` replaces scope checks by hand; typed SSE replaces `events()`'s `unknown`. |
| Stay on `openapi-typescript` + `openapi-fetch` | Two TS clients drift; no scopes. |

- `src/api/client.ts` becomes one `HashinboxClient` (`examples/dashboard.ts`): `onSession`
  keeps the token in `sessionStorage` (replaces `setToken`), `onUnauthorized` signs out
  (replaces `whenUnauthorized` and its `ASKS` list: the reason decides now);
  `call(client.GET('/v1/networks'))` becomes `networksList(api)`; the login page's
  `error.code === 'totp_required'` becomes `api.login(…)` and `error.reason`; `events()`
  becomes `logsStream(api, query)`, an `EventStream` stopped
  with `stop()` when its view closes.
- Kept hand-written: `exchange()` (the docs console sends arbitrary requests) and `openapi()`.
- `npm run api`, `schema.d.ts`, `openapi-fetch` and `openapi-typescript` are gone; types are
  imports from the package. `checkScopes: false`: the server's 403 stays what the UI shows.

## Plan

| Phase | Scope | Effort |
|---|---|---|
| 1 | **Done:** `packages/cligen` (parser, `Spec`, TS + Go + Rust generators for models and operations, golden and formatter tests), `build.rs` regeneration, minimal hand-written client files. **Left:** `#[scope::utoipa]` hole check; `scope.json` vectors; `TokenSession`/`KeySession`; transports (fetch, `net/http`, `reqwest`) | 6–8 days |
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
2. *(Decided)* A key's secret never goes over the wire: clients always sign short sessions with it.
3. **`?token=`** for `EventSource`: keep it a transport option (proposed), or drop it and support
   fetch-based streams only?
4. **TS integers:** `number` (proposed) or `bigint`/string for `int64`?
5. **Errors:** one `ApiError` with `Error` for every status (proposed, the spec has one error
   schema), or a per-status union per operation?
6. **Body-filled scope holes:** never (a route checks one scope; what it needs beyond it is a
   dependency, like `triggers` → `wallets`), or add `x-scopes` when the first one appears?
7. **Rust client and `scope`:** depend on the crate (publish `scope` + `scope-macros`), or a
   `scope` feature `macros` off by default?
8. **`src/lib.rs` for the server:** worth splitting later, so `clients` calls `api::openapi()`
   and drops the `docs/openapi.json` hop?
9. **cligen's home:** stays in this workspace, or its own repository once a second project uses
   it?
