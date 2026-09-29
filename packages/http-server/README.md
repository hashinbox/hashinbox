# http-server: hardened axum serving for application routers

## Why this crate exists

An HTTP API that holds wallets must not start in an unsafe shape: plaintext on
a public interface, or protected routes without authentication. This crate
turns those rules into configuration that is validated before any route is
served, and wraps an application's axum [`Router`](axum::Router) with the
matching layers: bearer authentication (constant-time comparison) and a
request body limit.

It owns no routes, handlers, response schemas, or business resources; those
belong to the application. The one exception is a name: the two health route
*paths* are published here ([`LIVENESS_PATH`], [`READINESS_PATH`]) so the
application and this crate cannot disagree about the HTTP contract, while the
handlers stay with the application that knows what "ready" means
(`apps/api/src/api/health.rs`). It is not an HTTP client (see `json-rpc`).

## Concepts

| Concept | Type | Notes |
|---|---|---|
| listener policy | [`Config`] | bind address, [`TransportSecurity`], [`BearerToken`], [`RequestLimits`] |
| transport rule | [`TransportSecurity`] | plaintext only on loopback; otherwise TLS terminated upstream |
| authentication rule | [`BearerToken`] | every protected route requires it; there is no mode that waives it |
| credential | [`BearerToken`] | redacted `Debug`; compared in constant time |
| health paths | [`LIVENESS_PATH`], [`READINESS_PATH`] | the route contract only; the application serves them, unauthenticated, with empty bodies |
| failures | [`ConfigError`] / [`ConfigErrorKind`], [`Error`] / [`ErrorKind`] | configuration vs. bind/serve |

[`Config::validate`] rejects a non-loopback bind without
[`TransportSecurity::TlsTerminatedUpstream`], and a configuration with no
[`BearerToken`]. [`protected_router`] and [`serve`] both validate first, so an
unauthenticated protected router cannot be built.

## Public API map

| Group | Items |
|---|---|
| construct | [`Config::new`], [`BearerToken::new`], [`RequestLimits::new`] |
| use | [`protected_router`] (layers only), [`serve`], [`LIVENESS_PATH`], [`READINESS_PATH`], [`UNAUTHORIZED_BODY`] |
| implement | nothing |
| errors | [`ConfigError`], [`ConfigErrorKind`], [`Error`], [`ErrorKind`] |

## Usage

```rust,no_run
use axum::{Router, http::StatusCode, routing::get};
use http_server::{
    BearerToken, Config, LIVENESS_PATH, RequestLimits, TransportSecurity, protected_router, serve,
};

async fn run(shutdown: impl Future<Output = ()> + Send + 'static)
    -> Result<(), Box<dyn std::error::Error>>
{
    let config = Config::new(
        "127.0.0.1:8080".parse()?,
        TransportSecurity::PlaintextLoopback,
        Some(BearerToken::new("change-me")?),
        RequestLimits::default(), // 1 MiB bodies
    );
    let routes = Router::new().route("/wallets", get(|| async { "[]" }));
    let router = protected_router(routes, &config)?
        // the application serves the health paths this crate names
        .route(LIVENESS_PATH, get(|| async { StatusCode::NO_CONTENT }));

    serve(router, &config, shutdown).await?;
    Ok(())
}
```

## Related crates

| Layer (`linter.toml`) | Crates | Relation |
|---|---|---|
| packages | none | depends only on `axum`, `tokio`, `constant_time_eq` |
| api, server | `payment-api`, `payment-server` | `payment-server` builds its [`Config`] from the environment; `payment-api` wraps its router with [`protected_router`] and serves [`LIVENESS_PATH`] and [`READINESS_PATH`] |

Design notes: [`docs/API.md`](../../docs/API.md) (authentication and health),
[`ARCHITECTURE.md`](../../ARCHITECTURE.md).
