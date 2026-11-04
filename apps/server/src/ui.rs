//! The browser face of the server: the React app built from `ui/`, compiled
//! into the binary and served without authentication. The page holds no data
//! of its own; it asks the JSON API through a client
//! generated from the OpenAPI document, with the bearer token the operator
//! types into it, so nothing here names a wallet, a chain or a store.

use axum::Router;
use http_server::{Cache, Embedded, Mount, Root};
use serde::Serialize;

/// Every file the server serves, by URL path: `ui/dist/` (written by
/// `npm run build` in `ui/` and committed, so building the server never needs
/// Node) at `/ui/…`, and `assets/` at `/assets/…` — a network asset's icon is
/// `/assets/crypto/<chain>.svg` (a native coin) or
/// `/assets/crypto/<chain>_<asset>.svg` (a token), lowercase. Listed by
/// `build.rs`.
const FILES: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/files.rs"));

/// The element the page reads its settings from; its body is replaced whole,
/// whatever whitespace the build left inside it.
const OPEN: &str = r#"<script id="settings" type="application/json">"#;

/// What the page reads at startup, as JSON. Every field is optional to the
/// page, which keeps its own defaults.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Settings {
    #[serde(skip_serializing_if = "Option::is_none")]
    brand: Option<String>,
    api_base: String,
}

/// The values a [`Settings`] is built from.
pub(crate) struct SettingsParams {
    /// The name in the top bar (`--brand`).
    pub(crate) brand: Option<String>,
    /// Where the page sends its API calls; empty means its own origin.
    pub(crate) api_base: String,
}

impl Settings {
    pub(crate) fn from_params(params: SettingsParams) -> Self {
        Self {
            brand: params.brand,
            api_base: params.api_base,
        }
    }

    /// The JSON, safe inside a `<script>` element: `<` never appears raw, so
    /// no value can close the element early.
    fn json(&self) -> String {
        serde_json::to_string(self)
            .expect("settings serialize")
            .replace('<', "\\u003c")
    }
}

impl Settings {
    /// The page with these settings written into its settings element.
    fn page(&self) -> Result<String, &'static str> {
        let (_, page) = FILES
            .iter()
            .find(|(url, _)| *url == "/ui/index.html")
            .ok_or("the UI is not built")?;
        let page = std::str::from_utf8(page).map_err(|_| "index.html is not UTF-8")?;
        let body = page
            .find(OPEN)
            .ok_or("index.html has no settings element")?
            + OPEN.len();
        let end = body
            + page[body..]
                .find("</script>")
                .ok_or("the settings element is unclosed")?;
        Ok(format!("{}{}{}", &page[..body], self.json(), &page[end..]))
    }

    /// `GET /` and every page path (`/ui/networks/...`, which the page
    /// reads) answer the page; an embedded file answers itself, and assets
    /// (icons) are cached a year. The settings are written into the page once,
    /// here, so the page never has to be rebuilt to change them.
    pub(crate) fn routes(&self) -> Result<Router, &'static str> {
        let ui = Mount::new("/ui", Embedded(FILES))
            .fallback(self.page()?)
            .root(Root::Page);
        let assets = Mount::new("/assets", Embedded(FILES)).cache(Cache::Immutable);
        Ok(ui.router().merge(assets.router()))
    }
}
