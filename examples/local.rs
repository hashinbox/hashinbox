//! The examples' non-chain setup: the document each scenario configures its
//! chains from, where its index file lives, and how it reaches a local node.
//!
//! This is the one file under `examples/` that names a chain crate, and it
//! names each exactly once, to register it — the same three lines
//! `apps/server/src/chains.rs` has. Everything a scenario says about a chain
//! it says in its own `config.toml`; `examples/tests/scenarios.rs` checks both
//! halves.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

use indexing::{NetworkEvent, NetworkState, SyncHealth, SyncMonitor, sqlite::Database};
use json_rpc::Client as _;
use sdk_config::{Document, Registry, Sections};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::{sync::watch, task::JoinSet};
use wallets::{
    Composition, RegistrationError, RegistrationFuture, SubmissionRegistrar, SubmissionTask,
};

/// The error every example returns.
pub type AnyError = Box<dyn std::error::Error + Send + Sync>;

/// The examples' own asset key: a closed set, because these examples run
/// exactly three assets and a document has to decode into it.
///
/// `wallets::Wallets<A>` is generic over this, so an application names its
/// assets however it likes — `payment_api::AssetTag` is the server's open
/// version. Each chain's section defaults to its own tag, so no `config.toml`
/// states one.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Asset {
    /// Bitcoin's native coin.
    Btc,
    /// Ethereum's native coin.
    Eth,
    /// Solana's native coin.
    Sol,
}

impl FromStr for Asset {
    type Err = AnyError;

    /// The same spelling the document uses, because it is the same decoder: a
    /// chain's `Config` defaults its asset with `A::from_str`, and a second
    /// hand-written table here could disagree with the derive above.
    fn from_str(tag: &str) -> Result<Self, Self::Err> {
        Ok(serde_json::from_value(Value::String(tag.to_owned()))?)
    }
}

/// The wallets a scenario can sign with, by the id it chose.
///
/// `wallets::Wallets` unifies factories and stores nothing, and the index owns
/// the durable list of what it observes, so the id -> wallet association is the
/// caller's (`docs/design/030 §8 (ex-029)` section 3.2). `payment_api::Signers` is the
/// application's version of this map; a scenario needs no more than a
/// `BTreeMap`.
#[derive(Default)]
pub struct Signers(BTreeMap<&'static str, Arc<dyn wallets::Wallet>>);

impl Signers {
    /// Files `wallet` under `id`.
    ///
    /// Call it after `index.watch` has returned: the durable watch row commits
    /// first, so a crash leaves a watch-only address rather than a key nothing
    /// observes.
    pub fn keep(&mut self, id: &'static str, wallet: Arc<dyn wallets::Wallet>) {
        self.0.insert(id, wallet);
    }

    /// The wallet of `id`.
    ///
    /// # Errors
    ///
    /// No wallet is held for `id`.
    pub fn get(&self, id: &str) -> Result<&Arc<dyn wallets::Wallet>, AnyError> {
        self.0
            .get(id)
            .ok_or_else(|| format!("no wallet is held for {id}").into())
    }

    /// Every held wallet, in id order.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &Arc<dyn wallets::Wallet>)> {
        self.0.iter().map(|(id, wallet)| (*id, wallet))
    }
}

/// One network's store: every indexing port a chain's index and watch list
/// write through, over the examples' index file.
pub type Repository = indexing::sqlite::Repository;

/// The placeholder a `config.toml` writes where the value can only come from
/// the node it is about to verify.
///
/// There is exactly one, and it is Solana's genesis hash: a
/// `solana-test-validator --reset` mints a new cluster on every run, so no
/// checked-in file can state it. [`Scenario::read`] asks the node of whichever
/// section wrote this for `getGenesisHash` and substitutes the answer before
/// the document is parsed. A deployment writes the hash it expects, and its
/// node is refused if it serves another.
pub const FROM_THE_NODE: &str = "<genesis-hash-from-the-node>";

/// One scenario's deployment document: `examples/<name>/config.toml`, read
/// once, with [`FROM_THE_NODE`] already resolved.
///
/// This is where every scenario states its chains. A `main.rs` names none:
/// the document names them, and [`Scenario::compose`] hands each named section
/// to the chain registered for it.
pub struct Scenario {
    name: String,
    text: String,
    sections: BTreeMap<String, Value>,
}

impl Scenario {
    /// Reads `examples/<name>/config.toml`. Run a scenario from the repository
    /// root, which is where that path resolves — the same place `.index-data`
    /// is made.
    ///
    /// # Errors
    ///
    /// The file is unreadable or is not TOML, a section that states
    /// [`FROM_THE_NODE`] states no endpoint, and whatever the node asked for
    /// the genesis hash reports.
    pub async fn read(name: &str) -> Result<Self, AnyError> {
        let path = format!("examples/{name}/config.toml");
        let mut text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|error| format!("{path}: {error}"))?;
        let sections: BTreeMap<String, Value> = Document::toml(text.clone()).parse()?;
        for section in sections.values() {
            if !section.to_string().contains(FROM_THE_NODE) {
                continue;
            }
            let genesis: String = Node::from_section(section)?
                .call("getGenesisHash", json!([]))
                .await?;
            text = text.replace(FROM_THE_NODE, &genesis);
        }
        let sections = Document::toml(text.clone()).parse()?;
        Ok(Self {
            name: name.to_owned(),
            text,
            sections,
        })
    }

    /// The node of one configured section, for a local-network call the SDK
    /// does not make: mining a block, asking for an airdrop.
    ///
    /// The name is the document's own, which is why a scenario may write it;
    /// a chain crate's name is `local.rs`'s alone.
    ///
    /// # Errors
    ///
    /// The document configures no such section, or it states no endpoint.
    pub fn node(&self, section: &str) -> Result<Node, AnyError> {
        Node::from_section(self.section(section)?)
    }

    /// One key of one configured section, decoded as `T`.
    ///
    /// Only the chain that owns a section knows what its keys mean, so this is
    /// not a door `sdk_config` opens. `custom_store` is the one scenario that
    /// needs it: it assembles its index by hand instead of from the document,
    /// and its networks must still come from that one source.
    ///
    /// # Errors
    ///
    /// The document configures no such section, the section states no such
    /// key, or the value is not a `T`.
    pub fn value<T: DeserializeOwned>(&self, section: &str, key: &str) -> Result<T, AnyError> {
        let value = self.section(section)?.get(key).ok_or_else(|| {
            format!(
                "examples/{}/config.toml: [{section}] states no {key}",
                self.name
            )
        })?;
        Ok(serde_json::from_value(value.clone())?)
    }

    /// One configured section, still raw.
    ///
    /// # Errors
    ///
    /// The document configures no such section.
    fn section(&self, name: &str) -> Result<&Value, AnyError> {
        self.sections
            .get(name)
            .ok_or_else(|| format!("examples/{}/config.toml has no [{name}]", self.name).into())
    }

    /// Both startup phases, in the one order that is safe: the document's
    /// sections are parsed and accepted, every configured chain's node
    /// identity is verified, the index file under `.index-data` is opened only
    /// then, and every chain composes itself over that one repository — one
    /// store, every network.
    ///
    /// # Errors
    ///
    /// A section no chain is registered for or that its own chain refuses, the
    /// first chain whose node serves another network, the store that cannot be
    /// opened after every identity held, and whatever a chain reports while
    /// building its index and factories.
    pub async fn compose(
        &self,
        registrar: &Arc<dyn SubmissionRegistrar>,
    ) -> Result<Composition<Asset>, AnyError> {
        let sections: Sections = Document::toml(self.text.clone()).parse()?;
        let chains = Self::registry().chains(&sections)?;
        for notice in chains.notices() {
            println!("{notice}");
        }
        let directory = DataDir::new()?;
        let file = format!("{}.db", self.name);
        // These examples put no `${NAME}` reference in a node URL, so nothing
        // stands in for one. A server resolves them from its environment.
        let secrets = BTreeMap::new();
        chains
            .compose(
                &secrets,
                || directory.open(&file),
                |store: &Database| Ok(Repository::new(store.clone())),
                registrar,
            )
            .await
    }

    /// Which chain owns which section of a scenario's document.
    ///
    /// The names are the document's, stated once, here: this is the whole of
    /// the coupling between `examples/` and a chain crate, and it is the same
    /// three lines `apps/server/src/chains.rs` has. A fourth chain is a fourth
    /// line, and every scenario's `config.toml` then owes it a section.
    fn registry() -> Registry<Asset, Repository> {
        let mut registry = Registry::new();
        registry.register::<chain_bitcoin::Config<Asset>>("bitcoin");
        registry.register::<chain_ethereum::Config<Asset>>("ethereum");
        registry.register::<chain_solana::Config<Asset>>("solana");
        registry
    }
}

/// Who runs background send work: a chain that finishes a send after the call
/// returns (solana confirms and recovers) hands the work to a
/// `SubmissionRegistrar`, so the application decides how long it may run.
///
/// `apps/server/src/submissions.rs` is the production version, which also
/// refuses new work once shutdown starts; this one owns the tasks so
/// [`Submissions::drain`] can wait for them before the process exits.
#[derive(Default)]
pub struct Submissions(Mutex<JoinSet<()>>);

impl SubmissionRegistrar for Submissions {
    fn register<'a>(&'a self, task: SubmissionTask) -> RegistrationFuture<'a> {
        Box::pin(async move {
            let mut tasks = self.0.lock().map_err(|_| RegistrationError::Closed)?;
            tasks.spawn(task.run());
            Ok(())
        })
    }
}

impl Submissions {
    /// Waits for every task registered so far; call it once, last.
    ///
    /// # Errors
    ///
    /// A registered task panicked, or another one poisoned the set.
    pub async fn drain(&self) -> Result<(), AnyError> {
        let mut tasks = std::mem::take(&mut *self.0.lock().map_err(|_| "poisoned submissions")?);
        while let Some(result) = tasks.join_next().await {
            result?;
        }
        Ok(())
    }
}

/// A local node, as the scenario's document states it.
///
/// There is no constructor from a URL: an endpoint is the document's to name,
/// and the linter forbids an example reading one from the environment.
pub struct Node(String);

impl Node {
    /// Request timeout of every example transport.
    const TIMEOUT: Duration = Duration::from_secs(15);

    /// The node one configured section names: the first URL of its endpoint
    /// list. These examples add no failover, so a local node's answer is the
    /// answer.
    fn from_section(section: &Value) -> Result<Self, AnyError> {
        let url = section
            .pointer("/rpc/endpoints/0")
            .and_then(Value::as_str)
            .ok_or("a section states rpc.endpoints = ['<url>']")?;
        Ok(Self(url.to_owned()))
    }

    /// The HTTP transport a chain's `RpcClient::connect` takes, for a scenario
    /// that builds its index by hand instead of from a `Config`.
    ///
    /// # Errors
    ///
    /// The transport rejects the endpoint.
    pub fn transport(&self) -> Result<json_rpc::Http, AnyError> {
        Ok(self
            .endpoint()
            .connect(&std::collections::BTreeMap::new())?)
    }

    /// This node, bounds included.
    fn endpoint(&self) -> json_rpc::Endpoint {
        json_rpc::Endpoint::new(&self.0).with_bounds(json_rpc::Bounds {
            timeout_ms: u64::try_from(Self::TIMEOUT.as_millis()).unwrap_or(u64::MAX),
            ..json_rpc::Bounds::default()
        })
    }

    /// One raw node call outside the SDK, for local-network setup such as
    /// mining or an airdrop.
    ///
    /// # Errors
    ///
    /// The transport fails, the node rejects the call, or the result does
    /// not decode as `T`.
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T, AnyError> {
        let request = json_rpc::Request::once(method, params);
        let result = self.transport()?.request(request).await?;
        Ok(result.map_err(json_rpc::CallError::from)?.deserialize()?)
    }
}

/// The directory of the examples' index files: `.index-data`, under the
/// directory a scenario is run from.
struct DataDir(PathBuf);

impl DataDir {
    /// `.index-data` under the current directory.
    ///
    /// # Errors
    ///
    /// The current directory is unavailable.
    fn new() -> Result<Self, AnyError> {
        Ok(Self(std::path::absolute(".index-data")?))
    }

    /// Opens (or creates) `file` in the directory, off the async runtime.
    ///
    /// # Errors
    ///
    /// The directory or the store cannot be opened.
    async fn open(&self, file: &str) -> Result<Database, AnyError> {
        let (dir, path) = (self.0.clone(), self.0.join(file));
        tokio::task::spawn_blocking(move || -> Result<_, AnyError> {
            std::fs::create_dir_all(&dir)?;
            Ok(indexing::sqlite::Database::open(path)?)
        })
        .await?
    }
}

/// The examples' synchronization monitor: it prints every network transition and
/// publishes the snapshot [`Readiness`] waits on.
///
/// `apps/server/src/readiness.rs` is the production version, which also serves
/// the health snapshot to the HTTP layer.
pub struct Health(watch::Sender<SyncHealth>);

/// Waits for every configured network to reach its node's tip.
pub struct Readiness(watch::Receiver<SyncHealth>);

impl Health {
    /// A monitor over `networks`, and the readiness it publishes.
    #[must_use]
    pub fn watching(networks: &[base::Network]) -> (Arc<Self>, Readiness) {
        let (snapshot, readiness) = watch::channel(SyncHealth::starting(networks));
        (Arc::new(Self(snapshot)), Readiness(readiness))
    }
}

impl SyncMonitor for Health {
    fn network_changed(&self, event: &NetworkEvent) {
        let health = &event.health;
        println!(
            "sync {}/{}: {:?} -> {:?}",
            health.network.chain, health.network.name, event.previous, health.state
        );
        self.0.send_modify(|snapshot| snapshot.apply(health));
    }
}

impl Readiness {
    /// Waits until every network has reached the tip its node reported.
    ///
    /// # Errors
    ///
    /// The supervisor stopped publishing.
    pub async fn ready(&mut self) -> Result<(), AnyError> {
        self.0
            .wait_for(|health| {
                health
                    .networks()
                    .iter()
                    .all(|row| row.state == NetworkState::Ready)
            })
            .await?;
        Ok(())
    }
}
