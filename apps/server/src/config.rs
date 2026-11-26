//! The command line: what the server needs before it can open the database
//! (its file and its binding), each flag with
//! an environment fallback. Everything else is in the database.

use std::{
    env,
    error::Error,
    net::SocketAddr,
    path::{self, PathBuf},
};

use anyhow::{Context as _, ensure};
use tracing::{Level, Metadata, Subscriber, level_filters::LevelFilter};
use tracing_subscriber::{
    EnvFilter, Layer,
    filter::{FilterExt as _, filter_fn},
    layer::SubscriberExt as _,
};

use crate::{
    db::Db,
    usecase::logs::{DbLayer, Feed, Line},
};

/// This build's version: `HASHINBOX_VERSION` at build time, else
/// `git describe --tags --always --dirty`, else the crate version (`build.rs`).
pub(crate) const VERSION: &str = env!("HASHINBOX_VERSION");

pub(crate) type AnyError = Box<dyn Error + Send + Sync>;

/// The command line.
#[derive(clap::Parser)]
#[command(
    name = "hashinbox",
    version = VERSION,
    args_conflicts_with_subcommands = true
)]
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
    /// Run the server; a new database file is created first.
    Serve(Serve),
}

/// The database file.
#[derive(clap::Args)]
pub(crate) struct Storage {
    /// The SQLite file; a relative path resolves against the working
    /// directory.
    #[arg(long, env = "HASHINBOX_DB", default_value = ".data/hashinbox.db")]
    db: PathBuf,
}

/// `serve`'s flags.
#[derive(clap::Args)]
pub(crate) struct Serve {
    #[command(flatten)]
    pub(crate) storage: Storage,
    /// The address the API listens on.
    #[arg(long, env = "HASHINBOX_BIND", default_value = "127.0.0.1:8080")]
    pub(crate) bind: SocketAddr,
    /// TLS is terminated by a proxy in front, so a non-loopback `bind` is
    /// accepted.
    #[arg(long, env = "HASHINBOX_TLS_TERMINATED_UPSTREAM")]
    tls_terminated_upstream: bool,
    /// The least severe event logged: off, error, warn, info, debug or
    /// trace.
    #[arg(long, env = "HASHINBOX_LOG_LEVEL", default_value = "info")]
    log_level: LevelFilter,
    /// One event's own level, `TARGET=LEVEL` (`rpc.http=debug`); repeatable.
    #[arg(long = "log", value_parser = target_level)]
    log: Vec<(String, LevelFilter)>,
    /// One event's level in the `log` table, `TARGET=LEVEL`; repeatable.
    /// The rest are kept from `debug`; a failed or broken `rpc.http.*` always is.
    #[arg(long = "log-db", value_parser = target_level)]
    log_db: Vec<(String, LevelFilter)>,
    /// A preset for an empty database: inline JSON (starting with `{`) or
    /// a JSON file's path, `{"root": {email, name, password}, "settings":
    /// {key: value}}`. Not read once a user exists.
    #[arg(long = "config", env = "HASHINBOX_CONFIG", hide_env_values = true)]
    pub(crate) bootstrap: Option<String>,
}

/// Parses `TARGET=LEVEL` the way `--log` and `--log-db` take it.
fn target_level(text: &str) -> Result<(String, LevelFilter), String> {
    let (target, level) = text
        .split_once('=')
        .ok_or_else(|| format!("`{text}` is not TARGET=LEVEL"))?;
    let level = level
        .parse()
        .map_err(|_| format!("`{level}` is not a log level"))?;
    Ok((target.to_owned(), level))
}

/// An `EnvFilter` at `level`, each of `targets` at its own, every span
/// let through so each event keeps its context.
fn filter<S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>>(
    level: LevelFilter,
    targets: &[(String, LevelFilter)],
) -> Result<impl tracing_subscriber::layer::Filter<S> + use<S>, AnyError> {
    let mut directives = level.to_string();
    for (target, level) in targets {
        directives.push_str(&format!(",{target}={level}"));
    }
    let env = EnvFilter::builder().parse(directives)?;
    Ok(env.or(filter_fn(|metadata: &Metadata<'_>| metadata.is_span())))
}

impl Storage {
    /// The file's absolute path: `--db` against the working directory.
    ///
    /// # Errors
    ///
    /// When the path is empty or does not resolve.
    pub(crate) fn path(&self) -> anyhow::Result<PathBuf> {
        ensure!(
            !self.db.as_os_str().is_empty(),
            "storage path must not be empty"
        );
        path::absolute(&self.db)
            .context("storage path is not resolvable against the working directory")
    }
}

impl Serve {
    /// Where the browser page sends its API calls, from `HASHINBOX_UI_API_BASE`.
    /// Unset or empty means the page's own origin, which is right whenever
    /// this server serves the page; set it when a proxy or another origin
    /// fronts the API.
    pub(crate) fn api_base() -> Result<String, AnyError> {
        let base = env::var("HASHINBOX_UI_API_BASE").unwrap_or_default();
        let base = base.trim().trim_end_matches('/');
        let absolute = base.starts_with("https://") || base.starts_with("http://");
        if !(base.is_empty() || absolute || base.starts_with('/')) {
            return Err(
                "HASHINBOX_UI_API_BASE must be an http(s) URL or a path starting with /".into(),
            );
        }
        Ok(base.to_owned())
    }

    /// The one subscriber, two layers, each with its own filter: every
    /// event at `--log-level` (or its `--log` level) as a line on stdout
    /// ([`Line`]); and every event from `debug` (or its `--log-db` level),
    /// and every failed or broken `rpc.http.*`, to the `log` table of `db`
    /// and to `feed` ([`DbLayer`]).
    ///
    /// # Errors
    ///
    /// A level that does not parse.
    pub(crate) fn subscriber(
        &self,
        db: &Db,
        feed: Feed,
    ) -> Result<impl Subscriber + use<>, AnyError> {
        let stdout = self.stdout()?;
        let failed_http = filter_fn(|metadata| {
            [rpc::events::RPC_HTTP_FAILED, rpc::events::RPC_HTTP_BROKEN]
                .contains(&metadata.target())
        });
        // Our events are named (`rpc.http.failed`, `index.step`); a library's are
        // its module path (`hyper_util::client`): kept from warn only, so
        // its connection chatter never fills the table.
        let ours = filter_fn(|metadata| {
            metadata.is_span()
                || !metadata.target().contains("::")
                || *metadata.level() <= Level::WARN
        });
        let kept = filter(LevelFilter::DEBUG, &self.log_db)?
            .and(ours)
            .or(failed_http);
        let database = DbLayer::new(db.clone(), feed).with_filter(kept);
        Ok(tracing_subscriber::registry().with(stdout).with(database))
    }

    /// [`Serve::subscriber`]'s stdout layer alone: what logs before the
    /// database, and its `log` table, are open.
    ///
    /// # Errors
    ///
    /// A level that does not parse.
    pub(crate) fn stdout_subscriber(&self) -> Result<impl Subscriber + use<>, AnyError> {
        Ok(tracing_subscriber::registry().with(self.stdout()?))
    }

    /// Every event at `--log-level` (or its `--log` level) as a line on
    /// stdout ([`Line`]).
    fn stdout<S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>>(
        &self,
    ) -> Result<impl Layer<S> + use<S>, AnyError> {
        Ok(tracing_subscriber::fmt::layer()
            .event_format(Line)
            .with_writer(std::io::stdout)
            .with_filter(filter(self.log_level, &self.log)?))
    }

    pub(crate) fn server(&self) -> Result<http_server::Config, AnyError> {
        let security = if self.tls_terminated_upstream {
            http_server::TransportSecurity::TlsTerminatedUpstream
        } else {
            http_server::TransportSecurity::PlaintextLoopback
        };
        let config = http_server::Config::new(
            self.bind,
            security,
            None,
            http_server::RequestLimits::default(),
        );
        config.validate()?;
        Ok(config)
    }
}
