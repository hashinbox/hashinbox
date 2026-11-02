//! The server's configuration document: HTTP binding, storage, the payments
//! networks. An RPC endpoint is its URL, credentials included.
//!
//! A `.toml` path is read as TOML, anything else as JSON. The `payments`
//! section is `payments::Document`, which this module never looks inside:
//! `crate::chains` says which chain owns which section name.

use std::{
    env,
    error::Error,
    net::SocketAddr,
    path::{self, Path, PathBuf},
};

use anyhow::{Context as _, ensure};
use serde::Deserialize;

use crate::db::Db;

pub(crate) type AnyError = Box<dyn Error + Send + Sync>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub(crate) bind: SocketAddr,
    /// The variable holding the API token; absent means no authorization,
    /// which only a loopback `bind` accepts.
    #[serde(default)]
    bearer_token_env: Option<String>,
    #[serde(default)]
    tls_terminated_upstream: bool,
    #[serde(default)]
    pub(crate) storage: Storage,
    /// Every network, each section naming its `chain`.
    #[serde(default)]
    pub(crate) payments: payments::Document,
    /// How the browser page presents itself; every key is optional.
    #[serde(default)]
    pub(crate) ui: Ui,
}

/// The `"storage"` section: `{"path": ...}`, defaulting to `payment.db`;
/// a relative path resolves against the working directory.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Storage {
    path: PathBuf,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            path: PathBuf::from("payment.db"),
        }
    }
}

impl Storage {
    /// Opens the configured file as one pool; applies no schema.
    ///
    /// # Errors
    ///
    /// When the path does not resolve or the pool does not open.
    pub(crate) fn open(&self) -> anyhow::Result<Db> {
        ensure!(
            !self.path.as_os_str().is_empty(),
            "storage path must not be empty"
        );
        let path = path::absolute(&self.path)
            .context("storage path is not resolvable against the working directory")?;
        Db::open(&path)
    }
}

/// The `[ui]` section: what the page shows that is the deployment's choice.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ui {
    /// The name in the top bar; the page has its own default.
    pub(crate) brand: Option<String>,
    /// Block explorer links, by chain, then network.
    #[serde(default)]
    pub(crate) explorers: crate::models::Explorers,
}

impl Config {
    pub(crate) async fn read(path: impl AsRef<Path>) -> Result<Self, AnyError> {
        let path = path.as_ref();
        let text = tokio::fs::read_to_string(path).await?;
        let config: Self = if path
            .extension()
            .is_some_and(|extension| extension == "toml")
        {
            toml::from_str(&text)?
        } else {
            serde_json::from_str(&text)?
        };
        if config
            .bearer_token_env
            .as_deref()
            .is_some_and(|name| name.trim().is_empty())
        {
            return Err("bearer-token environment name must not be empty".into());
        }
        Ok(config)
    }

    /// Where the browser page sends its API calls, from `PAYMENT_UI_API_BASE`.
    /// Unset or empty means the page's own origin, which is right whenever
    /// this server serves the page; set it when a proxy or another origin
    /// fronts the API.
    pub(crate) fn api_base() -> Result<String, AnyError> {
        let base = env::var("PAYMENT_UI_API_BASE").unwrap_or_default();
        let base = base.trim().trim_end_matches('/');
        let absolute = base.starts_with("https://") || base.starts_with("http://");
        if !(base.is_empty() || absolute || base.starts_with('/')) {
            return Err(
                "PAYMENT_UI_API_BASE must be an http(s) URL or a path starting with /".into(),
            );
        }
        Ok(base.to_owned())
    }

    pub(crate) fn server(&self) -> Result<http_server::Config, AnyError> {
        let token = match &self.bearer_token_env {
            Some(name) => Some(http_server::BearerToken::new(env::var(name)?)?),
            None => None,
        };
        let security = if self.tls_terminated_upstream {
            http_server::TransportSecurity::TlsTerminatedUpstream
        } else {
            http_server::TransportSecurity::PlaintextLoopback
        };
        let config = http_server::Config::new(
            self.bind,
            security,
            token,
            http_server::RequestLimits::default(),
        );
        config.validate()?;
        Ok(config)
    }
}
