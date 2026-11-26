//! Static files and single-page apps: a [`Mount`] serves the files of a
//! [`Source`] under one URL prefix, with configurable caching, media types,
//! extra headers, an optional fallback page and an optional `/` route.

use std::{
    borrow::Cow,
    collections::HashMap,
    future::Future,
    path::{Component, Path as FsPath, PathBuf},
    sync::Arc,
    time::Duration,
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::Path,
    http::{
        HeaderMap, HeaderName, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_TYPE},
    },
    response::{IntoResponse, Redirect, Response},
    routing::get,
};

/// Where a mount's files come from, keyed by full URL path (`/app.js`).
///
/// Implement it to serve from anywhere else; [`Embedded`] and [`Disk`] ship
/// with the crate.
pub trait Source: Send + Sync + 'static {
    /// The bytes of the file at URL `path`, or `None` when there is none.
    fn get(&self, path: &str) -> impl Future<Output = Option<Cow<'static, [u8]>>> + Send;
}

/// Files compiled into the binary: `(url path, bytes)` rows, e.g. written by a
/// build script with `include_bytes!`.
#[derive(Clone, Copy)]
pub struct Embedded(pub &'static [(&'static str, &'static [u8])]);

impl Source for Embedded {
    async fn get(&self, path: &str) -> Option<Cow<'static, [u8]>> {
        let (_, bytes) = self.0.iter().find(|(url, _)| *url == path)?;
        Some(Cow::Borrowed(*bytes))
    }
}

/// Files under a directory: URL `/app.js` reads `<root>/app.js`.
/// Paths with `..` or any other non-plain component are refused.
#[derive(Clone)]
pub struct Disk(pub PathBuf);

impl Source for Disk {
    async fn get(&self, path: &str) -> Option<Cow<'static, [u8]>> {
        let relative = FsPath::new(path.trim_start_matches('/'));
        if !relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        {
            return None;
        }
        let bytes = tokio::fs::read(self.0.join(relative)).await.ok()?;
        Some(Cow::Owned(bytes))
    }
}

/// A `Cache-Control` policy.
#[derive(Clone, Debug)]
pub enum Cache {
    /// `no-cache`: stored, but revalidated before every use.
    NoCache,
    /// `no-store`: never stored.
    NoStore,
    /// `public, max-age=31536000, immutable`: for content-hashed files.
    Immutable,
    /// `public, max-age=<seconds>`.
    MaxAge(Duration),
    /// Any other value, verbatim.
    Custom(HeaderValue),
}

impl Cache {
    fn value(&self) -> HeaderValue {
        match self {
            Self::NoCache => HeaderValue::from_static("no-cache"),
            Self::NoStore => HeaderValue::from_static("no-store"),
            Self::Immutable => HeaderValue::from_static("public, max-age=31536000, immutable"),
            Self::MaxAge(age) => format!("public, max-age={}", age.as_secs())
                .parse()
                .expect("a digit string is a header value"),
            Self::Custom(value) => value.clone(),
        }
    }
}

/// What `GET /` answers, besides the mount's own prefix.
#[derive(Clone, Debug, Default)]
pub enum Root {
    /// No `/` route (the default).
    #[default]
    Nothing,
    /// The mount's fallback page.
    Page,
    /// A temporary redirect to this path.
    Redirect(String),
}

/// One URL prefix served from a [`Source`]. Defaults: `no-cache`, the built-in
/// media types, no extra headers, no fallback (an unknown path is `404`), no
/// `/` route. [`Mount::router`] turns it into an axum [`Router`]; merge several.
pub struct Mount<S> {
    prefix: String,
    source: S,
    cache: Box<dyn Fn(&str) -> Cache + Send + Sync>,
    media: HashMap<String, HeaderValue>,
    headers: HeaderMap,
    fallback: Option<Bytes>,
    root: Root,
}

impl<S: Source> Mount<S> {
    /// Serves `source`'s files under `prefix` (`/assets`; empty or `/` for the
    /// whole site, where the API's own routes still win).
    pub fn new(prefix: &str, source: S) -> Self {
        Self {
            prefix: prefix.trim_end_matches('/').to_owned(),
            source,
            cache: Box::new(|_| Cache::NoCache),
            media: HashMap::new(),
            headers: HeaderMap::new(),
            fallback: None,
            root: Root::Nothing,
        }
    }

    /// One cache policy for every file.
    pub fn cache(self, cache: Cache) -> Self {
        self.cache_with(move |_| cache.clone())
    }

    /// A cache policy per file, chosen from its URL path.
    pub fn cache_with(mut self, rule: impl Fn(&str) -> Cache + Send + Sync + 'static) -> Self {
        self.cache = Box::new(rule);
        self
    }

    /// The `Content-Type` for files ending in `.<extension>`, over the
    /// built-in one.
    pub fn media_type(mut self, extension: &str, value: HeaderValue) -> Self {
        self.media.insert(extension.to_owned(), value);
        self
    }

    /// A header added to every response of this mount.
    pub fn header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.insert(name, value);
        self
    }

    /// Single-page app mode: the prefix itself and every unknown path under it
    /// answer `page` as HTML with `no-cache`. Pass the final page (any
    /// transform already applied).
    pub fn fallback(mut self, page: impl Into<Bytes>) -> Self {
        self.fallback = Some(page.into());
        self
    }

    /// What `GET /` answers.
    pub fn root(mut self, root: Root) -> Self {
        self.root = root;
        self
    }

    /// The routes: `<prefix>/{*path}`, plus `<prefix>` and `/` as configured.
    pub fn router(self) -> Router {
        let prefix = self.prefix.clone();
        let root = self.root.clone();
        let mount = Arc::new(self);
        let files = Arc::clone(&mount);
        let mut router = Router::new().route(
            &format!("{prefix}/{{*path}}"),
            get(move |Path(path): Path<String>| {
                let mount = Arc::clone(&files);
                async move { mount.file(&format!("{}/{path}", mount.prefix)).await }
            }),
        );
        let page = Arc::clone(&mount);
        let page = get(move || {
            let mount = Arc::clone(&page);
            async move { mount.page() }
        });
        if mount.fallback.is_some() && !prefix.is_empty() {
            router = router.route(&prefix, page.clone());
        }
        match root {
            Root::Nothing => router,
            Root::Page => router.route("/", page),
            Root::Redirect(target) => router.route(
                "/",
                get(move || async move { Redirect::temporary(&target) }),
            ),
        }
    }

    /// The file at URL `path`, else the fallback page, else `404`.
    async fn file(&self, path: &str) -> Response {
        let Some(bytes) = self.source.get(path).await else {
            return self.page();
        };
        let extension = path.rsplit_once('.').map_or("", |(_, extension)| extension);
        let media = self.media.get(extension).cloned().unwrap_or_else(|| {
            let (_, media) = MEDIA_TYPES
                .iter()
                .find(|(known, _)| *known == extension)
                .unwrap_or(&("", "application/octet-stream"));
            HeaderValue::from_static(media)
        });
        self.respond(Body::from(bytes), media, (self.cache)(path))
    }

    /// The fallback page, or `404` without one.
    fn page(&self) -> Response {
        let Some(page) = &self.fallback else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let media = HeaderValue::from_static("text/html; charset=utf-8");
        self.respond(Body::from(page.clone()), media, Cache::NoCache)
    }

    fn respond(&self, body: Body, media: HeaderValue, cache: Cache) -> Response {
        let mut response = Response::new(body);
        let headers = response.headers_mut();
        headers.insert(CONTENT_TYPE, media);
        headers.insert(CACHE_CONTROL, cache.value());
        headers.extend(self.headers.clone());
        response
    }
}

/// The built-in media types, by file extension.
const MEDIA_TYPES: &[(&str, &str)] = &[
    ("html", "text/html; charset=utf-8"),
    ("js", "text/javascript; charset=utf-8"),
    ("mjs", "text/javascript; charset=utf-8"),
    ("css", "text/css; charset=utf-8"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("ico", "image/x-icon"),
    ("json", "application/json"),
    ("map", "application/json"),
    ("woff2", "font/woff2"),
    ("txt", "text/plain; charset=utf-8"),
];

#[cfg(test)]
mod tests {
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    const FILES: &[(&str, &[u8])] = &[
        ("/app.js", b"js"),
        ("/app.wasm", b"wasm"),
        ("/assets/icon.svg", b"<svg/>"),
    ];

    async fn call(router: &Router, uri: &str) -> Response {
        let request = Request::get(uri).body(Body::empty()).expect("a request");
        router.clone().oneshot(request).await.expect("a response")
    }

    async fn body(response: Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a body")
            .to_vec()
    }

    fn spa() -> Router {
        Mount::new("", Embedded(FILES))
            .fallback("<html>index</html>")
            .root(Root::Page)
            .router()
            .merge(
                Mount::new("/assets", Embedded(FILES))
                    .cache(Cache::Immutable)
                    .router(),
            )
    }

    #[tokio::test]
    async fn an_embedded_file_is_served_with_its_type() {
        let response = call(&spa(), "/app.js").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );
        assert_eq!(response.headers()[CACHE_CONTROL], "no-cache");
        assert_eq!(body(response).await, b"js");
    }

    #[tokio::test]
    async fn an_unknown_page_path_and_the_root_answer_the_fallback() {
        let router = spa();
        for uri in ["/", "/networks", "/networks/btc", "/networks/"] {
            let response = call(&router, uri).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
            assert_eq!(body(response).await, b"<html>index</html>");
        }
    }

    #[tokio::test]
    async fn an_unknown_asset_is_not_found_and_a_known_one_is_immutable() {
        let router = spa();
        assert_eq!(
            call(&router, "/assets/nope.svg").await.status(),
            StatusCode::NOT_FOUND
        );
        let found = call(&router, "/assets/icon.svg").await;
        assert_eq!(found.headers()[CONTENT_TYPE], "image/svg+xml");
        assert_eq!(
            found.headers()[CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
    }

    #[tokio::test]
    async fn cache_rule_media_type_and_header_are_the_callers() {
        let router = Mount::new("", Embedded(FILES))
            .cache_with(|path| {
                if path.ends_with(".wasm") {
                    Cache::MaxAge(Duration::from_secs(60))
                } else {
                    Cache::NoStore
                }
            })
            .media_type("wasm", HeaderValue::from_static("application/wasm"))
            .header(
                HeaderName::from_static("x-frame-options"),
                HeaderValue::from_static("DENY"),
            )
            .router();
        let wasm = call(&router, "/app.wasm").await;
        assert_eq!(wasm.headers()[CONTENT_TYPE], "application/wasm");
        assert_eq!(wasm.headers()[CACHE_CONTROL], "public, max-age=60");
        assert_eq!(wasm.headers()["x-frame-options"], "DENY");
        let js = call(&router, "/app.js").await;
        assert_eq!(js.headers()[CACHE_CONTROL], "no-store");
    }

    #[tokio::test]
    async fn a_disk_file_is_served_and_traversal_is_refused() {
        let root = std::env::temp_dir().join(format!("http-server-static-{}", std::process::id()));
        std::fs::create_dir_all(root.join("site")).expect("a directory");
        std::fs::write(root.join("site/hello.txt"), "hi").expect("a file");
        std::fs::write(root.join("secret.txt"), "no").expect("a file");
        let router = Mount::new("/site", Disk(root.clone())).router();
        let found = call(&router, "/site/hello.txt").await;
        assert_eq!(found.headers()[CONTENT_TYPE], "text/plain; charset=utf-8");
        assert_eq!(body(found).await, b"hi");
        assert_eq!(Disk(root.clone()).get("/site/../secret.txt").await, None);
        let escaped = call(&router, "/site/..%2Fsecret.txt").await;
        assert_eq!(escaped.status(), StatusCode::NOT_FOUND);
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
