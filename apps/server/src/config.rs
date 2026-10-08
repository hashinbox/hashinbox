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

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn ethereum() -> Value {
        json!({
            "ethereum": {
                "network": "mainnet",
                "chain_id": 1,
                "rpc": { "endpoints": ["http://127.0.0.1:8545"] }
            }
        })
    }

    fn solana() -> Value {
        let node = json!({
            "url": "http://127.0.0.1:8899",
            "bounds": {"max_request_bytes": 1048576}
        });
        json!({
            "solana": {
                "network": "localnet",
                "genesis_hash": "11111111111111111111111111111111",
                "rpc": {"endpoints": [node], "repeat": "off"},
                "cadence": {
                    "poll_millis": 1000,
                    "batch_size": 100
                }
            }
        })
    }

    fn value(chains: Value) -> Value {
        json!({
            "bind": "127.0.0.1:3000",
            "bearer_token_env": "API_TOKEN",
            "chains": chains
        })
    }

    fn parse(value: Value) -> Result<Config, AnyError> {
        let config: Config = serde_json::from_value(value)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn storage_defaults_to_one_file_and_names_no_backend() {
        parse(value(solana())).expect("default storage");
        let mut explicit = value(solana());
        explicit["storage"] = json!({"path": "/tmp/payment.db"});
        parse(explicit).expect("explicit storage path");
        let mut external = value(solana());
        external["storage"] = json!({"postgres": {
            "url_env": "PAYMENT_DATABASE_URL", "schema": "payment", "max_connections": 8
        }});
        assert!(
            parse(external).is_err(),
            "one embedded file is the only index store this server composes"
        );
    }

    /// `"indexes"` configured chains and assets, never indexes; the key is
    /// `"chains"` and the old spelling is rejected outright.
    #[test]
    fn the_chain_section_is_named_chains() {
        let mut legacy = value(solana());
        legacy["indexes"] = legacy["chains"].take();
        legacy
            .as_object_mut()
            .expect("configuration object")
            .remove("chains");
        assert!(parse(legacy).is_err(), "`indexes` is replaced by `chains`");
        parse(value(solana())).expect("`chains` is the configured section");
    }

    #[test]
    fn empty_chains_allow_ui_setup_and_unknown_chains_are_rejected() {
        parse(value(json!({}))).expect("UI starts without configured nodes");
        let error = parse(value(json!({"dogecoin": {}})))
            .err()
            .expect("no chain is registered for it")
            .to_string();
        assert!(error.starts_with("dogecoin: "), "{error}");
        assert!(
            error.ends_with("configures bitcoin, ethereum, solana"),
            "{error}"
        );
    }

    /// The one caller `payments::config::Document::read` gained: a deployment's file
    /// is TOML, and each chain's section is the same shape as every other's.
    #[tokio::test]
    async fn a_toml_document_configures_the_same_chains_a_json_one_does() {
        let path = std::env::temp_dir().join("payment-server-config.toml");
        tokio::fs::write(
            &path,
            "bind = '127.0.0.1:3000'\n\
             bearer_token_env = 'API_TOKEN'\n\
             \n\
             [chains.solana]\n\
             network = 'localnet'\n\
             genesis_hash = '11111111111111111111111111111111'\n\
             rpc = { endpoints = ['http://127.0.0.1:8899'], repeat = 'off' }\n\
             cadence = { poll_millis = 1000, batch_size = 100 }\n",
        )
        .await
        .expect("a writable temporary directory");
        let config = Config::read(&path).await.expect("a TOML deployment");
        assert_eq!(
            config.chains().expect("one chain").networks(),
            parse(value(solana()))
                .expect("the same chain in JSON")
                .chains()
                .expect("one chain")
                .networks()
        );
    }

    #[test]
    fn rejects_removed_wallet_configuration() {
        let mut config = value(solana());
        config["wallets"] = json!([]);
        let error = parse(config)
            .err()
            .expect("wallet configuration is removed");
        assert!(error.to_string().contains("unknown field `wallets`"));
    }

    #[test]
    fn rejects_unknown_alias_database_and_forbidden_runtime_controls() {
        for (pointer, key) in [
            ("chains", "database"),
            ("solana", "database"),
            ("solana", "commitment"),
            ("solana", "priority_fee"),
            ("solana", "max_lag_slots"),
            ("solana", "reference_endpoint"),
            ("solana", "memo_program"),
            ("rpc", "endpoint"),
            ("rpc", "headers"),
            ("rpc", "timeout_seconds"),
            ("rpc", "max_response_bytes"),
            ("rpc", "reference_quorum"),
            ("rpc", "retry"),
            ("cadence", "freshness_mode"),
            ("cadence", "reorg_retention"),
        ] {
            for extra in [json!(1), Value::Null, json!(""), json!([]), json!({})] {
                let mut config = value(solana());
                let object = match pointer {
                    "chains" => &mut config["chains"],
                    "solana" => &mut config["chains"]["solana"],
                    "rpc" => &mut config["chains"]["solana"]["rpc"],
                    _ => &mut config["chains"]["solana"]["cadence"],
                };
                object
                    .as_object_mut()
                    .expect("fixture object")
                    .insert(key.to_owned(), extra);
                assert!(
                    parse(config).is_err(),
                    "{pointer}.{key} must be rejected for every value shape"
                );
            }
        }
        let mut chains = ethereum();
        chains["ethereum"]["database"] = json!("per-chain.db");
        assert!(parse(value(chains)).is_err());
    }

    /// Solana takes the one plural RPC shape: one node or several, never
    /// none and never another shape.
    #[test]
    fn solana_uses_the_one_rpc_shape_with_one_or_more_endpoints() {
        for endpoints in [Value::Null, json!("http://127.0.0.1:8899"), json!([])] {
            let mut config = value(solana());
            config["chains"]["solana"]["rpc"]["endpoints"] = endpoints;
            assert!(parse(config).is_err());
        }
        let mut config = value(solana());
        config["chains"]["solana"]["rpc"]["endpoints"] =
            json!(["http://127.0.0.1:8899", "http://127.0.0.1:8900"]);
        parse(config).expect("several Solana nodes");
    }
}
