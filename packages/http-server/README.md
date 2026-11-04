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

## Static files and single-page apps

A `Mount` serves the files of a `Source` under one URL prefix and builds an axum
`Router`; merge one per prefix. Sources key files by full URL path (`/ui/app.js`):
`Embedded` holds `(url path, bytes)` rows compiled in, `Disk` reads under a
directory (refusing `..`), and any other `Source` impl plugs in the same way.

```rust
use std::time::Duration;

use axum::{Router, http::{HeaderName, HeaderValue}};
use http_server::{Cache, Embedded, Mount, Root};

const FILES: &[(&str, &[u8])] = &[
    ("/ui/index.html", b"<html></html>"),
    ("/ui/app.wasm", b"\0asm"),
    ("/assets/logo.svg", b"<svg/>"),
];

let page = "<html>settings written here</html>".to_owned();
let router: Router = Mount::new("/ui", Embedded(FILES))
    .fallback(page)
    .root(Root::Page)
    .media_type("wasm", HeaderValue::from_static("application/wasm"))
    .cache_with(|path| {
        if path.ends_with(".wasm") { Cache::MaxAge(Duration::from_secs(3600)) } else { Cache::NoCache }
    })
    .header(HeaderName::from_static("x-frame-options"), HeaderValue::from_static("DENY"))
    .router()
    .merge(Mount::new("/assets", Embedded(FILES)).cache(Cache::Immutable).router());
```

- `fallback(page)`: single-page app mode; `<prefix>` and every unknown path
  under it answer `page` (HTML, `no-cache`). Without it an unknown path is `404`.
  Pass the final page; transform it (e.g. write settings in) before.
- `root(Root::Page | Root::Redirect(path) | Root::Nothing)`: what `GET /`
  answers; default `Nothing` (no route).
- `cache(Cache)` or `cache_with(|path| ..)`: `Cache-Control` per mount or per
  file: `NoCache` (default), `NoStore`, `Immutable`, `MaxAge(..)`, `Custom(..)`.
- `media_type(extension, value)`: overrides the built-in `Content-Type` map
  (html, js, mjs, css, svg, png, jpg, ico, json, map, woff2, txt; else
  `application/octet-stream`).
- `header(name, value)`: added to every response of the mount.

## Notes

- Only routes passed into `protected_router` require the token. A route added to
  the returned router, like the health route above, is public, because axum
  layers cover the routes that exist when the layer is applied.
