//! The scripted network: one bitcoin regtest network whose wallets,
//! broadcasts, balances and chain answer as its [`Script`] says, composed
//! from `base::testing` doubles.

use std::{cell::RefCell, sync::Arc, time::Duration};

use anyhow::Context as _;
use base::{
    Asset, AssetInfo, Assets, Chain, Future, Indexer, Line, Network, NetworkConfig, Networked,
    Service, Tracking, TransactionError, TransactionIndexer, WalletFactoryBroadcaster,
    WalletIndexer,
    testing::{Broadcasts, Keys, Served, Unasked, fixtures},
};

/// The scripted network.
pub(crate) fn network() -> Network {
    Network::new(Chain::new("bitcoin"), "regtest")
}

/// How the scripted network answers: its wallets sign as `keys` say, its
/// broadcasts answer as `broadcasts` says; the chain's balance reads, when
/// it has them; the chain its indexer reads (none: an empty one, or
/// [`days`] with a depth limit); how its payments are checked and bumped.
pub(crate) struct Script {
    pub(crate) keys: Keys,
    pub(crate) broadcasts: Arc<Broadcasts>,
    pub(crate) balances: Option<Arc<dyn base::Balances>>,
    pub(crate) chain: Option<Served>,
    pub(crate) tracking: Tracking,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            keys: Keys::new(network()),
            broadcasts: Arc::default(),
            balances: None,
            chain: None,
            tracking: Tracking::default(),
        }
    }
}

/// A chain whose tip is block 100, block `p` made `100 - p` days ago.
fn days() -> Served {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_secs();
    let block = |position: u64| {
        let mut block = fixtures::block(&network(), position, 0, vec![]);
        block.header.timestamp = Some(now - (100 - position) * 86_400);
        block
    };
    Served::new((0..=100).map(block).collect(), 0)
}

/// The fee rates of a network that prices no sends.
struct Unpriced;

impl Networked for Unpriced {
    fn network(&self) -> &Network {
        static NETWORK: std::sync::OnceLock<Network> = std::sync::OnceLock::new();
        NETWORK.get_or_init(network)
    }
}

impl base::FeeRateProvider for Unpriced {
    fn rate(&self) -> Future<'_, Result<base::FeeRate, TransactionError>> {
        unimplemented!("these tests price no sends")
    }
}

thread_local! {
    /// The script the next scripted network is built with; [`Scripted::new`]
    /// takes it, on the thread that registers the network.
    static SCRIPT: RefCell<Option<Arc<Script>>> = const { RefCell::new(None) };
}

/// The scripted network's chain, as its [`Script`] (taken from [`SCRIPT`])
/// says. Its line is the final block with balances, else its depth limit.
struct Scripted {
    script: Arc<Script>,
    max_depth: Option<u64>,
}

impl base::ServiceFactory for Scripted {
    const CHAIN: Chain = Chain::from_static("bitcoin");

    fn new(config: &NetworkConfig) -> anyhow::Result<Self> {
        Ok(Self {
            script: SCRIPT.take().context("no script is set")?,
            max_depth: config.max_depth,
        })
    }

    fn service(
        &self,
        storage: &base::Storage,
        client: Arc<dyn rpc::Client>,
    ) -> anyhow::Result<Service> {
        let chain = match (&self.script.chain, self.max_depth) {
            (Some(chain), _) => chain.clone(),
            (None, Some(_)) => days(),
            (None, None) => Served::empty(network()),
        };
        let balances = self.script.balances.clone();
        let line = match &balances {
            Some(_) => Line::Final,
            None => Line::Depth(self.max_depth.unwrap_or(u64::MAX)),
        };
        let chain: Arc<dyn base::Chained> = Arc::new(chain.with_line(line));
        let wallets = WalletIndexer::new(storage.indexes.clone(), chain.clone(), balances);
        let transactions =
            TransactionIndexer::new(storage.transaction_indexes.clone(), chain.clone(), None);
        let indexer = Indexer::new(
            chain,
            storage.blocks.clone(),
            vec![Arc::new(wallets), Arc::new(transactions)],
            Duration::from_secs(3_600),
        );
        Ok(Service {
            indexer: Arc::new(indexer),
            wallets: WalletFactoryBroadcaster {
                network: network(),
                factory: Arc::new(self.script.keys.clone()),
                broadcaster: self.script.broadcasts.clone(),
            },
            assets: Assets::from_iter([AssetInfo {
                asset: Asset::native(network()),
                decimals: 8,
                name: Some("Bitcoin".to_owned()),
                ticker: Some("BTC".to_owned()),
                display_decimals: None,
            }]),
            storage: storage.clone(),
            client,
            dated: self
                .max_depth
                .map(|_| -> Arc<dyn base::Dated> { Arc::new(days()) }),
            balances: self.script.balances.clone(),
            confirmer: None,
            confirmations: 0,
            tracking: self.script.tracking,
            rates: Arc::new(Unpriced),
        })
    }
}

/// The scripted network as registered; its nodes are read [`Unasked`].
fn config(max_depth: Option<u64>) -> NetworkConfig {
    NetworkConfig {
        network: network(),
        chain_id: Some(Unasked::CHAIN_ID.to_owned()),
        testnet: true,
        block_time: Duration::from_secs(600),
        confirmations: 0,
        max_depth,
        explorer_address: None,
        explorer_transaction: None,
        settings: serde_json::json!({}),
        assets: Vec::new(),
    }
}

/// The container of the scripted network over `db`.
pub(crate) fn container(
    db: &crate::db::Db,
    script: Arc<Script>,
) -> Arc<base::Container<crate::usecase::Transport>> {
    limited(db, script, None)
}

/// [`container`], with a depth limit of `max_depth` positions.
pub(crate) fn limited(
    db: &crate::db::Db,
    script: Arc<Script>,
    max_depth: Option<u64>,
) -> Arc<base::Container<crate::usecase::Transport>> {
    let container = base::Container::new(crate::usecase::sdk::Stores::storage(db.clone()));
    let config = config(max_depth);
    let node: Arc<dyn rpc::Client> = Arc::new(rpc::testing::Script::default());
    let transport = crate::usecase::Transport::from_clients::<Unasked>(&config, vec![node]);
    SCRIPT.set(Some(script));
    container.register::<Scripted>(config, transport);
    Arc::new(container)
}
