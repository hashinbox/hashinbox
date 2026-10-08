//! The server's configuration document: HTTP binding, storage, the chain
//! sections. An RPC endpoint is its URL, credentials included.
//!
//! `payments::config::Document::read` is the reader — TOML for a `.toml` path, JSON
//! for anything else — and the chain sections arrive as `payments::config::Sections`,
//! which this module never looks inside. `crate::chains` says which chain owns
//! which name; the chain itself says what its section means.

use std::{env, error::Error, net::SocketAddr, path::Path};

use payments::config::{Chains, Document, Sections};
use serde::Deserialize;

use crate::{
    asset::AssetTag,
    chains,
    storage::{self, Repository},
};

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
    pub(crate) storage: storage::Config,
    /// One section per chain, still raw: `crate::chains::registry` decides
    /// which chain parses which, and this module never looks inside one.
    #[serde(default)]
    pub(crate) chains: Sections,
    /// How the browser page presents itself; every key is optional.
    #[serde(default)]
    pub(crate) ui: Ui,
}

/// The `[ui]` section: what the page shows that is the deployment's choice.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ui {
    /// The name in the top bar; the page has its own default.
    pub(crate) brand: Option<String>,
}

impl Config {
    pub(crate) async fn read(path: impl AsRef<Path>) -> Result<Self, AnyError> {
        let config: Self = Document::read(path).await?.parse()?;
        config.validate()?;
        Ok(config)
    }

    /// Every configured chain, each section parsed and accepted by the chain
    /// that owns it. Nothing is contacted and nothing is opened.
    ///
    /// # Errors
    ///
    /// A section no chain is registered for, one that does not decode, or one
    /// its own chain refuses — every message naming the section at fault.
    pub(crate) fn chains(&self) -> Result<Chains<AssetTag, Repository>, AnyError> {
        let chains = chains::registry().chains(&self.chains)?;
        Ok(chains)
    }

    fn validate(&self) -> Result<(), AnyError> {
        if self
            .bearer_token_env
            .as_deref()
            .is_some_and(|name| name.trim().is_empty())
        {
            return Err("bearer-token environment name must not be empty".into());
        }
        self.storage.validate()?;
        self.chains()?;
        Ok(())
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
