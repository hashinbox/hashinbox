//! The server's tests, compiled into the binary's test target (the server has
//! no library): the suite (`cases`, over mock chains through `support`),
//! and, until it replaces them, the stores and reads on SQLite (`storage`),
//! the send job's first run (`send`) and its checks (`check`) and the
//! routes (`routes`) over one scripted network. Offline.

mod cases;
mod check;
mod pipe;
mod routes;
mod send;
mod storage;
mod support;

use std::{
    cell::RefCell,
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Context as _;
use base::{
    Address, Asset, AssetInfo, Assets, Blocks, Broadcaster, Builder, Chain, Chained, Fee, Future,
    Indexer, InfoClient, Line, Network, NetworkConfig, Networked, Service, Signer, Tips, Tracking,
    Transaction, TransactionError, TransactionErrorKind, TransactionId, TransactionIndexer,
    TransactionState, Transfer, Wallet, WalletFactory, WalletFactoryBroadcaster, WalletIndexer,
};
use crypto::{Curve, SecretBytes};

/// The test network.
fn network() -> Network {
    Network::new(Chain::new("bitcoin"), "regtest")
}

/// A new database file with every schema applied, and the directory that
/// holds it.
async fn database() -> (crate::db::Db, tempfile::TempDir) {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let db = crate::db::Db::open(&directory.path().join("test.db")).expect("the file opens");
    migrate(&db).await.expect("the schema applies");
    (db, directory)
}

/// Applies the schema and seed to `db`, then adds the test network: its
/// `network`, native `network_asset` and `network_index` rows.
async fn migrate(db: &crate::db::Db) -> anyhow::Result<()> {
    use diesel::connection::SimpleConnection as _;

    db.migrate().await?;
    db.write(|connection| {
        Ok(connection.batch_execute(
            "
            INSERT INTO network (chain, network, testnet, enabled, block_time_ms, confirmations)
            VALUES ('bitcoin', 'regtest', 1, 1, 600000, 0);
            INSERT INTO network_asset (chain, network, asset, kind, ticker, decimals)
            VALUES ('bitcoin', 'regtest', 'native', 'native', 'BTC', 8);
            INSERT INTO network_index (chain, network) VALUES ('bitcoin', 'regtest');",
        )?)
    })
    .await
}

/// What each broadcast answers, in order; the fee each built transaction
/// pays (none when zero); how many transactions were built; every signed
/// one, which a loaded state names; every broadcast envelope; the chain's
/// balance reads, when it has them; the chain its indexer reads (none: an
/// empty one); how its payments are checked and bumped; and how signing a
/// replacement fails (none: it signs).
#[derive(Default)]
struct Script {
    answers: Mutex<VecDeque<Result<(), TransactionErrorKind>>>,
    fee: u64,
    built: Mutex<u8>,
    signed: Mutex<Vec<Transaction>>,
    sent: Mutex<Vec<Vec<u8>>>,
    balances: Option<Arc<dyn base::Balances>>,
    chain: Option<Arc<dyn Chained>>,
    tracking: Tracking,
    bump: Option<TransactionErrorKind>,
}

/// The one wallet of the scripted network; its key is its address text.
#[derive(Clone)]
struct Fake {
    address: Address,
    script: Arc<Script>,
}

impl base::Addressed for Fake {
    fn address(&self) -> &Address {
        &self.address
    }
}

impl Networked for Fake {
    fn network(&self) -> &Network {
        &self.address.network
    }
}

impl Wallet for Fake {
    fn transaction(&self, _asset: &Asset) -> Result<Box<dyn Builder>, TransactionError> {
        Ok(Box::new(Build {
            wallet: self.clone(),
            state: None,
        }))
    }

    fn private_key(&self) -> (Curve, SecretBytes) {
        (
            Curve::Secp256k1,
            SecretBytes::new(self.address.value.as_bytes()),
        )
    }
}

impl Broadcaster for Fake {
    fn broadcast<'a>(
        &'a self,
        transaction: &'a Transaction,
    ) -> Future<'a, Result<(), TransactionError>> {
        let bytes = transaction.bytes.clone();
        Box::pin(async move {
            self.script.sent.lock().expect("the log").push(bytes);
            let answer = self
                .script
                .answers
                .lock()
                .expect("the script")
                .pop_front()
                .unwrap_or(Ok(()));
            answer.map_err(|kind| TransactionError::new(kind, "scripted"))
        })
    }
}

/// A builder of the wallet's transactions: a payment's txid and bytes are
/// the build count; a replacement's are those of the one it continues with
/// `+` after, its fee one more, unless the script fails it. Its state is
/// the txid it signed last.
struct Build {
    wallet: Fake,
    state: Option<String>,
}

impl Builder for Build {
    fn with_default_source(&mut self, _source: Signer) {}

    fn from_transfer(&mut self, _transfer: Transfer) -> Result<(), TransactionError> {
        Ok(())
    }

    fn with_fee(&mut self, _fee: Fee) {}

    fn sign(&mut self) -> Future<'_, Result<Transaction, TransactionError>> {
        Box::pin(async move {
            let signed = self.signed()?;
            self.state = Some(signed.id.id.clone());
            let log = &self.wallet.script.signed;
            log.lock().expect("the log").push(signed.clone());
            Ok(signed)
        })
    }

    fn save(&self) -> Result<serde_json::Value, TransactionError> {
        let state = self.state.clone().map(serde_json::Value::from);
        state.ok_or_else(|| TransactionError::failed("nothing signed"))
    }

    fn load(&mut self, state: &serde_json::Value) -> Result<(), TransactionError> {
        let Some(txid) = state.as_str() else {
            return Err(TransactionError::failed("not a txid"));
        };
        self.state = Some(txid.to_owned());
        Ok(())
    }
}

impl Build {
    /// The replacement of the transaction its state names, else a payment.
    fn signed(&self) -> Result<Transaction, TransactionError> {
        let wallet = &self.wallet;
        if let Some(txid) = &self.state {
            if let Some(kind) = wallet.script.bump {
                return Err(TransactionError::new(kind, "scripted"));
            }
            let log = wallet.script.signed.lock().expect("the log");
            let Some(existing) = log.iter().find(|signed| signed.id.id == *txid) else {
                return Err(TransactionError::failed("never signed"));
            };
            return Ok(replacement(existing));
        }
        let mut built = wallet.script.built.lock().expect("the counter");
        *built += 1;
        Ok(Transaction {
            id: TransactionId::new(network(), built.to_string()),
            state: TransactionState::Pending,
            order: None,
            legs: if wallet.script.fee == 0 {
                Vec::new()
            } else {
                vec![base::Leg::Fee {
                    address: wallet.address.clone(),
                    asset: Asset::native(network()),
                    amount: wallet.script.fee.into(),
                }]
            },
            bytes: vec![*built],
            expires_height: None,
        })
    }
}

/// `existing` at a fee one more: its txid and bytes with `+` after.
fn replacement(existing: &Transaction) -> Transaction {
    let txid = format!("{}+", existing.id.id);
    let legs = existing.legs.iter().cloned().map(|leg| match leg {
        base::Leg::Fee {
            address,
            asset,
            amount,
        } => base::Leg::Fee {
            address,
            asset,
            amount: amount + base::Decimal::from(1),
        },
        leg => leg,
    });
    Transaction {
        id: TransactionId::new(network(), txid.clone()),
        legs: legs.collect(),
        bytes: txid.into_bytes(),
        ..existing.clone()
    }
}

/// Restores a [`Fake`] from its address text; generates `generated`.
struct Factory(Arc<Script>);

impl WalletFactory for Factory {
    fn generate(&self) -> Future<'_, anyhow::Result<SecretBytes>> {
        Box::pin(async { Ok(SecretBytes::new(b"generated")) })
    }

    fn from_pk(&self, secret: SecretBytes) -> Future<'_, anyhow::Result<Arc<dyn Wallet>>> {
        let script = self.0.clone();
        Box::pin(async move {
            let text = String::from_utf8(secret.as_bytes().to_vec())?;
            let wallet: Arc<dyn Wallet> = Arc::new(Fake {
                address: Address::new(network(), text),
                script,
            });
            Ok(wallet)
        })
    }

    /// The key text is the address text.
    fn from_pk_text(&self, text: &str) -> Future<'_, anyhow::Result<Arc<dyn Wallet>>> {
        self.from_pk(SecretBytes::new(text.as_bytes()))
    }

    fn parse_address(&self, text: &str) -> anyhow::Result<Address> {
        Ok(Address::new(network(), text))
    }

    fn transaction(&self, _asset: &Asset) -> Result<Box<dyn Builder>, TransactionError> {
        Err(TransactionError::failed("unused"))
    }

    fn fees(&self) -> Arc<dyn base::FeeEstimator> {
        unimplemented!("these tests price no sends")
    }
}

/// A chain that has produced nothing.
struct Empty;

impl Networked for Empty {
    fn network(&self) -> &Network {
        static NETWORK: std::sync::OnceLock<Network> = std::sync::OnceLock::new();
        NETWORK.get_or_init(network)
    }
}

impl base::FeeRateProvider for Empty {
    fn rate(&self) -> Future<'_, Result<base::FeeRate, TransactionError>> {
        unimplemented!("these tests price no sends")
    }
}

impl Chained for Empty {
    fn tips(&self) -> Future<'_, anyhow::Result<Tips>> {
        Box::pin(async {
            Ok(Tips {
                produced: 0,
                confirmed: 0,
            })
        })
    }

    fn block(&self, _position: u64) -> Future<'_, anyhow::Result<Option<base::Block>>> {
        Box::pin(async { Ok(None) })
    }

    fn blocks(&self, _start: u64, _end: u64) -> Future<'_, anyhow::Result<Blocks>> {
        Box::pin(async { Ok(Blocks::new(network())) })
    }
}

/// A chain whose tip is block 100, block `p` made `100 - p` days ago: what
/// the scripted network reads its tip and finds its blocks by time with,
/// when it has a depth limit.
struct Days;

impl Networked for Days {
    fn network(&self) -> &Network {
        Empty.network()
    }
}

impl Chained for Days {
    fn tips(&self) -> Future<'_, anyhow::Result<Tips>> {
        Box::pin(async {
            Ok(Tips {
                produced: 100,
                confirmed: 100,
            })
        })
    }

    fn block(&self, _position: u64) -> Future<'_, anyhow::Result<Option<base::Block>>> {
        Box::pin(async { Ok(None) })
    }
}

impl base::Dated for Days {
    fn time(&self, position: u64) -> Future<'_, anyhow::Result<Option<u64>>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after 1970")
            .as_secs();
        Box::pin(async move { Ok(Some(now - (100 - position) * 86_400)) })
    }
}

/// `.0`, its line `.1`: a test chain with the line a network's config gives.
struct Lined(Arc<dyn Chained>, Line);

impl Networked for Lined {
    fn network(&self) -> &Network {
        self.0.network()
    }
}

impl Chained for Lined {
    fn tips(&self) -> Future<'_, anyhow::Result<Tips>> {
        self.0.tips()
    }

    fn block(&self, position: u64) -> Future<'_, anyhow::Result<Option<base::Block>>> {
        self.0.block(position)
    }

    fn blocks(&self, start: u64, end: u64) -> Future<'_, anyhow::Result<Blocks>> {
        self.0.blocks(start, end)
    }

    fn line(&self) -> Line {
        self.1
    }
}

/// One node of the scripted network: regtest at tip 0, whatever it is.
struct Node(Arc<dyn rpc::Client>);

impl rpc::Client for Node {
    fn request<'a>(
        &'a self,
        request: rpc::Request,
    ) -> rpc::BoxFuture<'a, Result<rpc::RawJson, rpc::Error>> {
        self.0.request(request)
    }

    fn batch<'a>(
        &'a self,
        batch: rpc::Batch,
    ) -> rpc::BoxFuture<'a, Result<Vec<rpc::Answer>, rpc::Error>> {
        self.0.batch(batch)
    }

    fn broadcast<'a>(
        &'a self,
        request: rpc::Request,
    ) -> rpc::BoxFuture<'a, Result<(), rpc::Error>> {
        self.0.broadcast(request)
    }
}

impl InfoClient for Node {
    fn new(node: Arc<dyn rpc::Client>) -> Self {
        Self(node)
    }

    fn tip(&self) -> Future<'_, anyhow::Result<u64>> {
        Box::pin(async { Ok(0) })
    }

    fn info(&self) -> Future<'_, anyhow::Result<base::NodeInfo>> {
        Box::pin(async {
            Ok(base::NodeInfo {
                chain_id: "regtest".to_owned(),
                tip: 0,
            })
        })
    }
}

thread_local! {
    /// The script the next scripted network is built with; [`Scripted::new`]
    /// takes it, on the thread that registers the network.
    static SCRIPT: RefCell<Option<Arc<Script>>> = const { RefCell::new(None) };
}

/// The scripted network's chain: its wallets answer as its [`Script`]
/// (taken from [`SCRIPT`]) says; with a depth limit, it reads its tip and
/// finds blocks by time over [`Days`] (its script's chain, when set). Its
/// line is the final block with balances, else its depth limit.
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
            (None, Some(_)) => Arc::new(Days),
            (None, None) => Arc::new(Empty),
        };
        let balances = self.script.balances.clone();
        let line = match &balances {
            Some(_) => Line::Final,
            None => Line::Depth(self.max_depth.unwrap_or(u64::MAX)),
        };
        let chain: Arc<dyn Chained> = Arc::new(Lined(chain, line));
        let wallets = WalletIndexer::new(storage.indexes.clone(), chain.clone(), balances);
        let transactions =
            TransactionIndexer::new(storage.transaction_indexes.clone(), chain.clone(), None);
        let indexer = Indexer::new(
            chain,
            storage.blocks.clone(),
            vec![Arc::new(wallets), Arc::new(transactions)],
            Duration::from_secs(3_600),
        );
        let wallet = Fake {
            address: Address::new(network(), "unused"),
            script: self.script.clone(),
        };
        Ok(Service {
            indexer: Arc::new(indexer),
            wallets: WalletFactoryBroadcaster {
                network: network(),
                factory: Arc::new(Factory(self.script.clone())),
                broadcaster: Arc::new(wallet),
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
                .map(|_| -> Arc<dyn base::Dated> { Arc::new(Days) }),
            balances: self.script.balances.clone(),
            confirmer: None,
            confirmations: 0,
            tracking: self.script.tracking,
            rates: Arc::new(Empty),
        })
    }
}

/// The scripted network as stored.
fn config(max_depth: Option<u64>) -> NetworkConfig {
    NetworkConfig {
        network: network(),
        chain_id: None,
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
fn container(
    db: &crate::db::Db,
    script: Arc<Script>,
) -> Arc<base::Container<crate::usecase::Transport>> {
    limited(db, script, None)
}

/// [`container`], with a depth limit of `max_depth` positions.
fn limited(
    db: &crate::db::Db,
    script: Arc<Script>,
    max_depth: Option<u64>,
) -> Arc<base::Container<crate::usecase::Transport>> {
    let container = base::Container::new(crate::usecase::sdk::Stores::storage(db.clone()));
    let config = config(max_depth);
    let node: Arc<dyn rpc::Client> = Arc::new(rpc::testing::Script::default());
    let transport = crate::usecase::Transport::from_clients::<Node>(&config, vec![node]);
    SCRIPT.set(Some(script));
    container.register::<Scripted>(config, transport);
    Arc::new(container)
}
