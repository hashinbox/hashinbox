//! The server's tests, compiled into the binary's test target (the server has
//! no library): the suite (`cases`, over mock chains through `support`),
//! and, until it replaces them, the stores and reads on SQLite (`storage`),
//! the send job (`send`) and the routes (`routes`) over one scripted
//! network. Offline.

mod cases;
mod pipe;
mod routes;
mod send;
mod storage;
mod support;

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use base::{
    Address, Asset, AssetInfo, Assets, Blocks, Broadcaster, Builder, Chain, Chained, Cluster,
    Future, Indexer, InfoClient, Network, Networked, Policy, Service, Signer, Tips, Transaction,
    TransactionError, TransactionErrorKind, TransactionId, TransactionState, Transactions,
    Transfer, Wallet, WalletFactory, WalletFactoryBroadcaster,
};
use crypto::{Curve, SecretBytes};
use payments::Payments;

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
/// pays (none when zero); how many transactions were built; and every
/// broadcast envelope.
#[derive(Default)]
struct Script {
    answers: Mutex<VecDeque<Result<(), TransactionErrorKind>>>,
    fee: u64,
    built: Mutex<u8>,
    sent: Mutex<Vec<Vec<u8>>>,
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
            transfers: 0,
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

/// A builder of one transaction per transfer, its bytes the build count.
struct Build {
    wallet: Fake,
    transfers: usize,
}

impl Builder for Build {
    fn with_default_source(&mut self, _source: Signer) {}

    fn transfer(&mut self, _transfer: Transfer) -> Result<(), TransactionError> {
        self.transfers += 1;
        Ok(())
    }

    fn sign<'a>(self: Box<Self>) -> Future<'a, Result<Transactions, TransactionError>>
    where
        Self: 'a,
    {
        Box::pin(async move {
            let mut signed = Vec::new();
            for _ in 0..self.transfers {
                let mut built = self.wallet.script.built.lock().expect("the counter");
                *built += 1;
                signed.push(Transaction {
                    id: TransactionId::new(network(), built.to_string()),
                    state: TransactionState::Pending,
                    order: None,
                    legs: if self.wallet.script.fee == 0 {
                        Vec::new()
                    } else {
                        vec![base::Leg::Fee {
                            address: self.wallet.address.clone(),
                            asset: Asset::native(network()),
                            amount: self.wallet.script.fee.into(),
                        }]
                    },
                    bytes: vec![*built],
                    expires: None,
                });
            }
            Ok(signed.into())
        })
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
/// the scripted network finds its blocks by time with, when it has a
/// history limit.
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

/// The scripted network's configuration, and its history limit (found by
/// time over [`Days`]).
struct Config(Arc<Script>, Option<Duration>);

impl base::ServiceFactory for Config {
    fn network(&self) -> Network {
        network()
    }

    fn info(&self, nodes: Vec<Arc<dyn rpc::Client>>) -> anyhow::Result<Arc<dyn InfoClient>> {
        let nodes = nodes
            .into_iter()
            .map(|node| Arc::new(Node(node)) as Arc<dyn InfoClient>)
            .collect();
        Ok(Arc::new(Cluster::new(nodes, Policy::default())?))
    }

    fn build<'a>(
        &'a self,
        storage: &'a base::Storage,
        client: Arc<dyn rpc::Client>,
    ) -> Future<'a, anyhow::Result<Service>> {
        Box::pin(async move {
            let indexer = Indexer::new(
                Arc::new(Empty),
                storage.blocks.clone(),
                storage.indexes.clone(),
                Duration::from_secs(3_600),
            );
            let wallet = Fake {
                address: Address::new(network(), "unused"),
                script: self.0.clone(),
            };
            Ok(Service {
                indexer: Arc::new(indexer),
                wallets: WalletFactoryBroadcaster {
                    network: network(),
                    factory: Arc::new(Factory(self.0.clone())),
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
                dated: self.1.map(|_| -> Arc<dyn base::Dated> { Arc::new(Days) }),
                max_history: self.1,
                confirmations: 0,
            })
        })
    }
}

/// The payments container of the scripted network over `db`.
async fn payments(db: &crate::db::Db, script: Arc<Script>) -> Arc<Payments> {
    limited(db, script, None).await
}

/// [`payments`], with a history limit of `max_history`.
async fn limited(
    db: &crate::db::Db,
    script: Arc<Script>,
    max_history: Option<Duration>,
) -> Arc<Payments> {
    let stores = Arc::new(crate::usecase::sdk::Stores::new(db.clone()));
    let payments = Payments::builder()
        .with_network(Config(script, max_history), Vec::new())
        .with_client(network(), Arc::new(rpc::testing::Script::default()))
        .with_storage(base::Storage {
            blocks: stores.clone(),
            indexes: stores.clone(),
            inputs: stores.clone(),
            wallets: stores.clone(),
            holdings: stores,
        })
        .build()
        .await
        .expect("the scripted network builds");
    Arc::new(payments)
}
