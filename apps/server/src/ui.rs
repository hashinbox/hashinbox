//! The browser face of the server: the React app built from `ui/`, compiled
//! into the binary and served without authentication. The page holds no data
//! of its own; it asks the JSON API through a client
//! generated from the OpenAPI document, with the bearer token the operator
//! types into it, so nothing here names a wallet, a chain or a store.

use axum::{
    Router,
    http::header::{CONTENT_TYPE, HeaderValue},
    response::Html,
    routing::get,
};
use serde::Serialize;

// `npm run build` in `ui/` writes these; they are committed so that building
// the server never needs Node. The styles travel inside the script.
const PAGE: &str = include_str!("../ui/dist/index.html");
const SCRIPT: &str = include_str!("../ui/dist/app.js");
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
    /// The name in the top bar, from the configuration's `[ui]` section.
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
        let body = PAGE
            .find(OPEN)
            .ok_or("index.html has no settings element")?
            + OPEN.len();
        let end = body
            + PAGE[body..]
                .find("</script>")
                .ok_or("the settings element is unclosed")?;
        Ok(format!("{}{}{}", &PAGE[..body], self.json(), &PAGE[end..]))
    }

    /// `GET /` and the script the page loads. The settings are written into
    /// the page once, here, so the page never has to be rebuilt to change them.
    pub(crate) fn routes(&self) -> Result<Router, &'static str> {
        let page: &'static str = self.page()?.leak();
        let script = || async {
            let media = HeaderValue::from_static("text/javascript; charset=utf-8");
            ([(CONTENT_TYPE, media)], SCRIPT)
        };
        Ok(Router::new()
            .route("/", get(move || async move { Html(page) }))
            .route("/ui/app.js", get(script)))
    }
}
