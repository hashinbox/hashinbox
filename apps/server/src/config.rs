//! The server's configuration document: HTTP binding, storage, the chain
//! sections and wallets imported at startup. Secrets come from named
//! environment variables, never from the file.
//!
//! `sdk_config::Document::read` is the reader — TOML for a `.toml` path, JSON
//! for anything else — and the chain sections arrive as `sdk_config::Sections`,
//! which this module never looks inside. `crate::chains` says which chain owns
//! which name; the chain itself says what its section means.

use std::{collections::BTreeSet, env, error::Error, net::SocketAddr, path::Path};

use sdk_config::{Chains, Document, Sections};
use serde::{Deserialize, Deserializer, de};

use crate::{
    api::{AssetTag, Signers, Wallets},
    chains,
    storage::{self, Repository},
};

pub(crate) type AnyError = Box<dyn Error + Send + Sync>;

/// The process environment, as the values an endpoint URL's `${NAME}`
/// references stand for.
///
/// `packages/json-rpc` takes this seam rather than reading the environment
/// itself: `linter.toml` allows `std::env::var` only here and in `storage.rs`,
/// and the workspace's `unsafe_code = "forbid"` would make an
/// environment-reading crate untestable. `storage.rs` takes the same seam as a
/// lookup closure for the database URL.
// linter:disable rust/empty-struct -- Stateless: the process environment is the state it reads.
pub(crate) struct Environment;

impl json_rpc::Secrets for Environment {
    fn value(&self, name: &str) -> Option<String> {
        env::var(name).ok()
    }
}

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
    #[serde(default)]
    pub(crate) wallets: Vec<ConfiguredWallet>,
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
        let mut ids = BTreeSet::new();
        for wallet in &self.wallets {
            if wallet.id.trim().is_empty() || wallet.secret_env.trim().is_empty() {
                return Err("wallet ID and secret environment name must not be empty".into());
            }
            if !ids.insert(&wallet.id) {
                return Err("configured wallet IDs must be unique".into());
            }
        }
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

/// Explicit observation start for an environment-imported wallet.
/// Zero means genesis; `fresh` starts after the current checkpoint.
#[derive(Clone, Copy)]
enum Start {
    Fresh,
    At(u64),
}

impl From<Start> for indexing::Birthday {
    fn from(value: Start) -> Self {
        match value {
            Start::Fresh => Self::Fresh,
            Start::At(position) => Self::At(base::BlockPosition(position)),
        }
    }
}

impl<'de> Deserialize<'de> for Start {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Stated {
            Position(u64),
            Sentinel(String),
        }
        match Stated::deserialize(deserializer)? {
            Stated::Position(position) => Ok(Self::At(position)),
            Stated::Sentinel(text) if text == "fresh" => Ok(Self::Fresh),
            Stated::Sentinel(text) => Err(de::Error::invalid_value(
                de::Unexpected::Str(&text),
                &"\"fresh\" or a non-negative block position",
            )),
        }
    }
}

/// A wallet imported at every startup from a secret held in the
/// environment, indexed from its explicit birthday.
///
/// `start` is a native block position or `"fresh"`; `0` means genesis.
/// HTTP creation and watching always choose fresh observation.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfiguredWallet {
    id: String,
    asset: AssetTag,
    secret_env: String,
    start: Start,
}

impl ConfiguredWallet {
    pub(crate) fn asset(&self) -> &AssetTag {
        &self.asset
    }

    /// Builds the wallet, durably stores its key, publishes its watch, then
    /// installs the process's signing handle. Interrupted publication leaves
    /// the saved key private until the configured import is retried.
    ///
    /// The index is asked for one capability, `indexing::Watching`, because
    /// registering an observation is the whole of what an import does with it.
    ///
    /// The one key text every supported chain accepts is 64 hexadecimal
    /// characters, so the decode is chain-neutral and the asset's factory
    /// checks the length and the curve.
    pub(crate) async fn import(
        self,
        wallets: &Wallets,
        index: &dyn indexing::Watching,
        signers: &Signers,
        registry: &std::sync::Arc<crate::registry::Registry>,
    ) -> Result<(), AnyError> {
        let text = zeroize::Zeroizing::new(
            env::var(&self.secret_env)
                .map_err(|_| "configured wallet secret environment variable is unavailable")?,
        );
        let secret = wallets::SecretBytes::from_hex(&text)
            .map_err(|_| "configured wallet secret is not hexadecimal key material")?;
        let retained = wallets::SecretBytes::new(secret.as_bytes());
        let wallet = wallets
            .from(&self.asset, secret)
            .await
            .map_err(Self::disabled)?;
        registry.keep(&self.id, &self.asset, retained).await?;
        if signers.get(&self.id).is_some() {
            return Ok(());
        }
        index
            .watch(&self.id, wallet.as_ref(), self.start.into())
            .await?;
        signers.keep(self.id, wallet)?;
        Ok(())
    }

    /// An unregistered tag is a configuration error, not a wallet error.
    fn disabled(error: wallets::WalletError) -> AnyError {
        if error.kind == wallets::WalletErrorKind::Unsupported {
            return "configured wallet references a disabled asset".into();
        }
        error.into()
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

    fn value(chains: Value, wallets: Value) -> Value {
        json!({
            "bind": "127.0.0.1:3000",
            "bearer_token_env": "API_TOKEN",
            "chains": chains,
            "wallets": wallets
        })
    }

    fn parse(value: Value) -> Result<Config, AnyError> {
        let config: Config = serde_json::from_value(value)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn storage_defaults_to_one_file_and_names_no_backend() {
        parse(value(solana(), json!([]))).expect("default storage");
        let mut explicit = value(solana(), json!([]));
        explicit["storage"] = json!({"path": "/tmp/payment.db"});
        parse(explicit).expect("explicit storage path");
        let mut external = value(solana(), json!([]));
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
        let mut legacy = value(solana(), json!([]));
        legacy["indexes"] = legacy["chains"].take();
        legacy
            .as_object_mut()
            .expect("configuration object")
            .remove("chains");
        assert!(parse(legacy).is_err(), "`indexes` is replaced by `chains`");
        parse(value(solana(), json!([]))).expect("`chains` is the configured section");
    }

    #[test]
    fn empty_chains_allow_ui_setup_and_unknown_chains_are_rejected() {
        parse(value(json!({}), json!([]))).expect("UI starts without configured nodes");
        let error = parse(value(json!({"dogecoin": {}}), json!([])))
            .err()
            .expect("no chain is registered for it")
            .to_string();
        assert!(error.starts_with("dogecoin: "), "{error}");
        assert!(
            error.ends_with("configures bitcoin, ethereum, solana"),
            "{error}"
        );
    }

    /// The one caller `sdk_config::Document::read` gained: a deployment's file
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
            parse(value(solana(), json!([])))
                .expect("the same chain in JSON")
                .chains()
                .expect("one chain")
                .networks()
        );
    }

    #[test]
    fn wallet_validation_preserves_field_and_duplicate_precedence() {
        let valid = json!({
            "id": "treasury",
            "asset": "eth",
            "secret_env": "UNREAD_SECRET_ENVIRONMENT",
            "start": 1
        });
        let missing_secret = json!({
            "id": "other",
            "asset": "usdc",
            "secret_env": " ",
            "start": 1
        });
        for (wallets, expected) in [
            (
                json!([valid.clone(), missing_secret]),
                "wallet ID and secret environment name must not be empty",
            ),
            (
                json!([valid.clone(), valid.clone()]),
                "configured wallet IDs must be unique",
            ),
        ] {
            let error = parse(value(ethereum(), wallets))
                .err()
                .expect("first wallet violation must win");
            assert_eq!(error.to_string(), expected);
        }
        parse(value(ethereum(), json!([valid]))).expect("an open asset tag");
    }

    #[test]
    fn rejects_malformed_tags_and_height_only_birthdays() {
        for wallet in [
            json!({"id": "t", "asset": "ETH", "secret_env": "S", "start": 1}),
            json!({"id": "t", "asset": "eth", "secret_env": "S", "start_height": 1}),
            json!({"id": "t", "asset": "eth", "secret_env": "S", "start_position": 1}),
            json!({"id": "t", "asset": "eth", "secret_env": "S", "start": "tip"}),
            json!({"id": "t", "asset": "eth", "secret_env": "S"}),
        ] {
            assert!(parse(value(ethereum(), json!([wallet]))).is_err());
        }
    }

    /// The gap this closes: a bare integer could name genesis and could not
    /// name "this key has never been used", so a configured wallet with no
    /// past had to claim one.
    #[test]
    fn a_configured_wallet_can_state_a_position_or_that_it_is_fresh() {
        for start in [json!(0), json!(842_000), json!("fresh")] {
            let wallet = json!({
                "id": "treasury",
                "asset": "eth",
                "secret_env": "UNREAD_SECRET_ENVIRONMENT",
                "start": start,
            });
            parse(value(ethereum(), json!([wallet]))).expect("a stated birthday");
        }
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
                let mut config = value(solana(), json!([]));
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
        assert!(parse(value(chains, json!([]))).is_err());
    }

    /// Solana takes the one plural RPC shape and constrains it to one node.
    #[test]
    fn rejects_a_solana_section_without_exactly_one_endpoint() {
        for endpoints in [
            Value::Null,
            json!("http://127.0.0.1:8899"),
            json!([]),
            json!(["http://127.0.0.1:8899", "http://127.0.0.1:8900"]),
        ] {
            let mut config = value(solana(), json!([]));
            config["chains"]["solana"]["rpc"]["endpoints"] = endpoints;
            assert!(parse(config).is_err());
        }
    }
}
