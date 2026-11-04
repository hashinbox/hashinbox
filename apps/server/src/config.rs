//! The command line: what the server needs before it can open the database
//! (its file, its binding, its token and the page's brand), each flag with
//! an environment fallback. Everything else is in the database.

use std::{
    env,
    error::Error,
    net::SocketAddr,
    path::{self, PathBuf},
};

use anyhow::{Context as _, ensure};

use crate::db::Db;

pub(crate) type AnyError = Box<dyn Error + Send + Sync>;

/// The command line.
#[derive(clap::Parser)]
#[command(name = "payment-server", args_conflicts_with_subcommands = true)]
pub(crate) struct Cli {
    /// Print the OpenAPI document instead of serving, so a typed client can
    /// be generated without a node, a database, or a bound port.
    #[arg(long)]
    pub(crate) openapi: bool,
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(clap::Subcommand)]
pub(crate) enum Command {
    /// Run the server.
    Serve(Serve),
    /// Create the schema and its starting networks in a new database file.
    Migrate(Storage),
}

/// The database file.
#[derive(clap::Args)]
pub(crate) struct Storage {
    /// The SQLite file; a relative path resolves against the working
    /// directory.
    #[arg(long, env = "PAYMENT_DB", default_value = ".data/payment.db")]
    db: PathBuf,
}

/// `serve`'s flags.
#[derive(clap::Args)]
pub(crate) struct Serve {
    #[command(flatten)]
    pub(crate) storage: Storage,
    /// The address the API listens on.
    #[arg(long, env = "PAYMENT_BIND", default_value = "127.0.0.1:8080")]
    pub(crate) bind: SocketAddr,
    /// The variable holding the API token; absent means no authorization,
    /// which only a loopback `bind` accepts.
    #[arg(long, env = "PAYMENT_BEARER_TOKEN_ENV")]
    bearer_token_env: Option<String>,
    /// TLS is terminated by a proxy in front, so a non-loopback `bind` is
    /// accepted.
    #[arg(long, env = "PAYMENT_TLS_TERMINATED_UPSTREAM")]
    tls_terminated_upstream: bool,
    /// The name in the page's top bar; the page has its own default.
    #[arg(long, env = "PAYMENT_UI_BRAND")]
    pub(crate) brand: Option<String>,
}

impl Storage {
    /// Opens the file as one pool; applies no schema.
    ///
    /// # Errors
    ///
    /// When the path does not resolve or the pool does not open.
    pub(crate) fn open(&self) -> anyhow::Result<Db> {
        ensure!(
            !self.db.as_os_str().is_empty(),
            "storage path must not be empty"
        );
        let path = path::absolute(&self.db)
            .context("storage path is not resolvable against the working directory")?;
        Db::open(&path)
    }
}

impl Serve {
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
        let token = match self.bearer_token_env.as_deref().map(str::trim) {
            Some("") => return Err("bearer-token environment name must not be empty".into()),
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
