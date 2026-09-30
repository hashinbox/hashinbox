# http-server

Wraps an axum `Router` with bearer authentication and a request body limit, then
serves it. The configuration is validated first, so a listener without a token,
or a plaintext listener on a public interface, never starts.

## Usage

```rust,no_run
use axum::{Router, http::StatusCode, routing::get};
use http_server::{
    BearerToken, Config, LIVENESS_PATH, RequestLimits, TransportSecurity, protected_router, serve,
};

async fn run(
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::new(
        "127.0.0.1:8080".parse()?,
        TransportSecurity::PlaintextLoopback,
        Some(BearerToken::new("change-me")?),
        RequestLimits::default(),
    );
    let routes = Router::new().route("/wallets", get(|| async { "[]" }));
    let router = protected_router(routes, &config)?
        .route(LIVENESS_PATH, get(|| async { StatusCode::NO_CONTENT }));

    serve(router, &config, shutdown).await?;
    Ok(())
}
```

## Notes

- Only routes passed into `protected_router` require the token. A route added to
  the returned router, like the health route above, is public, because axum
  layers cover the routes that exist when the layer is applied.
