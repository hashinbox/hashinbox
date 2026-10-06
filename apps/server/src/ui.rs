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

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    use super::*;

    async fn fetch(path: &str) -> (StatusCode, String, String) {
        let response = Settings::from_params(SettingsParams {
            brand: None,
            api_base: String::new(),
        })
        .routes()
        .expect("the built page has a settings element")
        .oneshot(Request::get(path).body(Body::empty()).expect("request"))
        .await
        .expect("response");
        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .map(|value| value.to_str().expect("ascii").to_owned())
            .unwrap_or_default();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        (
            status,
            content_type,
            String::from_utf8(body.to_vec()).expect("utf-8"),
        )
    }

    #[tokio::test]
    async fn the_page_and_its_two_files_are_served_without_a_token() {
        let (status, content_type, body) = fetch("/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("text/html"), "{content_type}");
        assert!(body.contains("<title>Tally</title>"));
        assert!(body.contains("/ui/app.js"));

        let (status, content_type, body) = fetch("/ui/app.js").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            content_type.starts_with("text/javascript"),
            "{content_type}"
        );
        assert!(body.contains("/v1/wallets"));
    }

    #[test]
    fn the_page_carries_its_settings_and_no_value_can_close_the_script() {
        let settings = Settings::from_params(SettingsParams {
            brand: Some("</script><b>".to_owned()),
            api_base: "https://x".to_owned(),
        });
        let json = settings.json();
        assert!(!json.contains('<'), "{json}");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed["brand"], "</script><b>");
        assert_eq!(parsed["apiBase"], "https://x");
        let written = settings
            .page()
            .expect("the built page has a settings element");
        assert!(
            written.contains(&format!("{OPEN}{json}</script>")),
            "{written}"
        );
    }

    #[test]
    fn the_bundle_names_only_routes_the_api_serves() {
        let served = [
            "/health",
            "/v1/assets",
            "/v1/networks",
            "/v1/transactions",
            "/v1/wallets",
        ];
        // Every path literal in the bundle; the minifier writes them in double
        // quotes, so each starts at a `"/`.
        let named: Vec<&str> = SCRIPT
            .match_indices("\"/")
            .filter_map(|(at, _)| SCRIPT[at + 1..].split('"').next())
            .filter(|path| path.starts_with("/v1/") || *path == "/health")
            .collect();
        for path in &named {
            assert!(
                served.iter().any(|route| path.starts_with(route)),
                "the page asks for {path}, which the API does not serve"
            );
        }
        assert!(
            named.len() >= served.len() - 1,
            "the bundle names {} routes; was it built?",
            named.len()
        );
    }
}
