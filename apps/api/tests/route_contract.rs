//! The public route contract over test doubles of the wallet, factory, and
//! index abstractions: every route reaches wallets only through `Wallets`.

use std::sync::{Arc, Mutex};

use axum::{
    Router,
    body::{Body, Bytes},
    http::{Request, StatusCode},
};
use base::{
    Address, BlockHash, BlockHeight, BlockPosition, BlockRef, Decimal, SignRequest, Signer,
    TransactionId,
};
use http_body_util::BodyExt;
use indexing::{
    BoxFuture, ChainName, Checkpoint, Holder, IndexError, IndexErrorKind, MovementId, Network,
    NetworkHealth, NetworkState, ObservedTransaction, Start, SyncHealth, TransactionPage,
    TransactionRef, TransactionStatus, ValueMovement, Watch, Watching,
};
use payment_api::{AssetTag, Payments, Signers, router};
use serde_json::{Value, json};
use tokio::sync::watch;
use tower::ServiceExt;
use wallets::{
    AddressParser, FactoryProfile, FutureResult, Generation, SecretBytes, SendFuture, Sender,
    Transfer, WalletFactory, Wallets,
};

const TOKEN: &str = "route-contract-secret";

#[derive(Default)]
struct Calls {
    transfers: Mutex<Vec<Decimal>>,
    broadcasts: Mutex<usize>,
    batches: Mutex<Vec<usize>>,
}

/// One fixture network's index at height 8, whose every address history holds
/// one confirmed 1.5-coin output to that address.
///
/// One of these per network — a runnable index owns exactly one — composed into
/// the `indexing::Index` the routes hold. The watch list is shared between them
/// so an id is unique across the whole composition, exactly as it is in
/// production, where one repository serves every network.
struct FixtureIndex {
    network: Network,
    watched: Arc<Mutex<Vec<Watch>>>,
}

/// What the routes are handed for the three fixture networks: the index they
/// watch on, and the two per-network query handles they read through.
///
/// Three values, because that is the production shape: an `indexing::Index`
/// routes watches and checkpoints, while a page and a balance each come from the
/// handle that owns the holding's network — `indexing::ObservedHistory` and one
/// network's fold there, one of these units here.
struct Composed {
    index: Arc<indexing::Index>,
    history: Arc<dyn indexing::History>,
    balance: Arc<dyn indexing::Balances>,
}

impl FixtureIndex {
    /// The three fixture networks behind one index and one handle per query,
    /// over one watch list.
    fn composed(networks: Vec<Network>) -> Composed {
        let watched = Arc::new(Mutex::new(Vec::new()));
        let readers = networks
            .into_iter()
            .map(|network| {
                Arc::new(Self {
                    network,
                    watched: Arc::clone(&watched),
                })
            })
            .collect::<Vec<_>>();
        let routed = Arc::new(Readers(readers.clone()));
        let units = readers
            .into_iter()
            .map(|unit| unit as Arc<dyn indexing::Indexing>)
            .collect();
        let index = indexing::Index::of(units).expect("the fixture networks are disjoint");
        Composed {
            index: Arc::new(index),
            history: routed.clone() as Arc<dyn indexing::History>,
            balance: routed as Arc<dyn indexing::Balances>,
        }
    }
}

/// Every fixture network's query readers, keyed by the network each owns: what
/// `wallets::Composition` collects in production, standing in for it here.
///
/// One value satisfies both narrow capabilities because one fixture network
/// answers both, exactly as one `indexing::Service` does in production; the
/// routes still hold them as two handles and never as one fat surface.
struct Readers(Vec<Arc<FixtureIndex>>);

impl Readers {
    /// The unit that owns `of`'s network, or the refusal every read of an
    /// unconfigured network gives.
    fn owner(&self, of: &dyn Holder) -> Result<&Arc<FixtureIndex>, IndexError> {
        let network = &of.address().network;
        self.0
            .iter()
            .find(|reader| &reader.network == network)
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::NetworkMismatch,
                    "index network is not configured",
                    false,
                )
            })
    }
}

impl indexing::History for Readers {
    fn history<'a>(
        &'a self,
        of: &'a dyn Holder,
        page: indexing::Page,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
        match self.owner(of) {
            Ok(reader) => reader.history(of, page),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }
}

impl indexing::Balances for Readers {
    fn balance<'a>(
        &'a self,
        of: &'a dyn Holder,
    ) -> BoxFuture<'a, Result<indexing::Balance, IndexError>> {
        match self.owner(of) {
            Ok(reader) => reader.balance(of),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }
}

impl indexing::Indexing for FixtureIndex {
    fn network(&self) -> &Network {
        &self.network
    }

    fn listen(&self, _listener: Arc<dyn indexing::Listener>) {}

    fn sync<'a>(&'a self) -> BoxFuture<'a, Result<indexing::SyncStatus, IndexError>> {
        Box::pin(async { unreachable!("routes never synchronize") })
    }
}

/// The earliest start this fixture index accepts, standing in for
/// `SyncPolicy::max_backfill_blocks`: a caller-supplied birthday further below
/// the checkpoint than the index will read is refused by the call that made
/// it, which is what stops one from wedging readiness later.
const EARLIEST_START: u64 = 5;

impl Watching for FixtureIndex {
    fn watch<'a>(
        &'a self,
        id: &'a str,
        subject: &'a dyn Holder,
        start: Start,
    ) -> BoxFuture<'a, Result<Watch, IndexError>> {
        if matches!(start, Start::At(position) if position.0 < EARLIEST_START) {
            return Box::pin(async {
                Err(IndexError::new(
                    IndexErrorKind::InvalidRequest,
                    format!("the earliest acceptable start is {EARLIEST_START}"),
                    false,
                ))
            });
        }
        let watch = Watch {
            id: id.to_owned(),
            address: subject.address().clone(),
            asset: subject.asset().clone(),
            start: match start {
                Start::At(position) => position,
                Start::Fresh => BlockPosition(9),
            },
        };
        let mut watched = self.watched.lock().expect("fixture watch list");
        if watched
            .iter()
            .any(|row| row.address == watch.address || row.id == watch.id)
        {
            return Box::pin(async {
                Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "this address is already watched",
                    false,
                ))
            });
        }
        watched.push(watch.clone());
        drop(watched);
        Box::pin(async move { Ok(watch) })
    }

    fn unwatch<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Watch>, IndexError>> {
        let mut watched = self.watched.lock().expect("fixture watch list");
        let removed = watched
            .iter()
            .position(|row| row.id == id)
            .map(|index| watched.remove(index));
        drop(watched);
        Box::pin(async move { Ok(removed) })
    }

    fn watched<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
        let rows = self
            .watched
            .lock()
            .expect("fixture watch list")
            .iter()
            .filter(|row| row.address.network == *network)
            .cloned()
            .collect();
        Box::pin(async move { Ok(rows) })
    }
}

/// Cancelling re-anchors every row of the network that starts below the
/// checkpoint's successor, which is what a settled range leaves behind.
impl indexing::Backfilling for FixtureIndex {
    fn cancel<'a>(&'a self, network: &'a Network) -> BoxFuture<'a, Result<Vec<Watch>, IndexError>> {
        let mut watched = self.watched.lock().expect("fixture watch list");
        let mut cancelled = Vec::new();
        for row in watched.iter_mut() {
            if row.address.network == *network && row.start < BlockPosition(9) {
                row.start = BlockPosition(9);
                cancelled.push(row.clone());
            }
        }
        drop(watched);
        Box::pin(async move { Ok(cancelled) })
    }
}

impl Checkpoint for FixtureIndex {
    fn checkpoint<'a>(
        &'a self,
        _network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async { Ok(Some(block(8))) })
    }
}

impl indexing::History for FixtureIndex {
    fn history<'a>(
        &'a self,
        of: &'a dyn Holder,
        _page: indexing::Page,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
        let network = of.address().network.clone();
        let observed = ObservedTransaction {
            transaction_id: TransactionRef::new(network, "fixture-observed"),
            status: TransactionStatus::Confirmed {
                block: block(7),
                confirmations: 2,
            },
            movements: vec![ValueMovement::output(
                MovementId("output-0".to_owned()),
                of.asset().clone(),
                150_000_000_u64.into(),
                Some(of.address().clone()),
            )],
            fee: None,
        };
        Box::pin(async move {
            Ok(TransactionPage {
                checkpoint: Some(block(8)),
                transactions: vec![observed.clone()],
                pending: vec![ObservedTransaction {
                    transaction_id: TransactionRef::new(
                        observed.transaction_id.network.clone(),
                        "fixture-pending",
                    ),
                    status: TransactionStatus::Pending {
                        block: block(9),
                        confirmations: 1,
                    },
                    ..observed
                }],
                next: None,
                completeness: indexing::Completeness::Incomplete,
            })
        })
    }
}

/// One network folds the balance for any holder, with or without a key behind
/// it. Every amount is atomic, as `indexing` can only produce.
impl indexing::Balances for FixtureIndex {
    fn balance<'a>(
        &'a self,
        _of: &'a dyn Holder,
    ) -> BoxFuture<'a, Result<indexing::Balance, IndexError>> {
        Box::pin(async {
            Ok(indexing::Balance {
                amount: 1_250_000_000_u64.into(),
                observed_at: Some(block(8)),
                pending: indexing::PendingValue {
                    incoming: 25_000_000_u64.into(),
                    outgoing: indexing::Decimal::zero(),
                    block: Some(block(9)),
                },
                completeness: indexing::Completeness::Incomplete,
            })
        })
    }
}

/// Metadata of the fixture chains' native coin.
const COIN: base::Asset = base::Asset::new(
    base::ChainNetwork::new(
        base::ChainName::from_static("Fixture"),
        "fixture",
        base::NetworkKind::Mainnet,
        "FIX",
    ),
    "Fixture Coin",
    "FIX",
    8,
);

struct FixtureFactory {
    network: Network,
    calls: Arc<Calls>,
    generation_error: Option<wallets::WalletErrorKind>,
}

impl WalletFactory for FixtureFactory {
    fn validate(&self, wallet: &dyn wallets::Wallet) -> Result<(), wallets::WalletError> {
        if (wallet as &dyn std::any::Any)
            .downcast_ref::<FixtureWallet>()
            .is_some_and(|wallet| Arc::ptr_eq(&self.calls, &wallet.calls))
        {
            return Ok(());
        }
        Err(wallets::WalletError::new(
            wallets::WalletErrorKind::Unsupported,
            "incompatible fixture wallet",
        ))
    }

    /// The wallet at `fixture-address`, or `fixture-<hex>` for other keys.
    fn create<'a>(&'a self, secret: SecretBytes) -> FutureResult<'a, Arc<dyn wallets::Wallet>> {
        let value = match secret.as_bytes() {
            [7, ..] => "fixture-address".to_owned(),
            bytes => format!("fixture-{:02x}", bytes[0]),
        };
        let wallet = FixtureWallet {
            address: Address::new(self.network.clone(), value),
            asset: base::AssetId::native(self.network.chain.clone()),
            calls: Arc::clone(&self.calls),
        };
        Box::pin(async move { Ok(Arc::new(wallet) as Arc<dyn wallets::Wallet>) })
    }

    fn generate(&self) -> FutureResult<'_, Generation> {
        if let Some(kind) = self.generation_error {
            return Box::pin(async move {
                Err(wallets::WalletError::new(
                    kind,
                    "fixture wallet generation failed",
                ))
            });
        }
        let wallet = self.create(SecretBytes::new([7_u8; 32]));
        Box::pin(async move {
            Ok(Generation {
                wallet: wallet.await?,
                material: SecretBytes::new([7_u8; 32]),
            })
        })
    }
}

impl AddressParser for FixtureFactory {
    /// Accepts only `fixture-` text, in any case.
    fn parse_address(&self, text: &str) -> Result<Address, wallets::WalletError> {
        let canonical = text.to_lowercase();
        if !canonical.starts_with("fixture-") || canonical == "fixture-" {
            return Err(wallets::WalletError::new(
                wallets::WalletErrorKind::InvalidAddress,
                "fixture destination is invalid",
            ));
        }
        Ok(Address::new(self.network.clone(), canonical))
    }
}

impl FactoryProfile for FixtureFactory {
    fn network(&self) -> &Network {
        &self.network
    }

    fn asset(&self) -> base::AssetId {
        base::AssetId::native(self.network.chain.clone())
    }

    fn authorship(&self) -> Option<Arc<dyn wallets::Authorship>> {
        Some(Arc::new(FixtureSender {
            calls: Arc::clone(&self.calls),
        }))
    }
}

struct FixtureSender {
    calls: Arc<Calls>,
}

impl wallets::Authorship for FixtureSender {
    fn author(
        &self,
        wallet: &Arc<dyn wallets::Wallet>,
    ) -> Result<Arc<dyn wallets::Author>, wallets::WalletError> {
        let wallet = (wallet.as_ref() as &dyn std::any::Any)
            .downcast_ref::<FixtureWallet>()
            .ok_or_else(|| {
                wallets::WalletError::new(
                    wallets::WalletErrorKind::Unsupported,
                    "not a fixture wallet",
                )
            })?;
        Ok(Arc::new(FixtureWallet {
            address: wallet.address.clone(),
            asset: wallet.asset.clone(),
            calls: Arc::clone(&wallet.calls),
        }))
    }
}

impl Sender for FixtureSender {
    fn send<'a>(&'a self, transfers: Vec<Transfer>) -> SendFuture<'a> {
        let calls = Arc::clone(&self.calls);
        Box::pin(async move {
            calls
                .batches
                .lock()
                .expect("fixture batch lock")
                .push(transfers.len());
            Ok(vec![TransactionId::new("fixture-batch")])
        })
    }
}

struct FixtureWallet {
    address: Address,
    asset: base::AssetId,
    calls: Arc<Calls>,
}

impl Holder for FixtureWallet {
    fn address(&self) -> &Address {
        &self.address
    }

    fn asset(&self) -> &base::AssetId {
        &self.asset
    }
}

impl Signer for FixtureWallet {
    fn sign<'a>(&'a self, _request: SignRequest) -> base::SignFuture<'a> {
        Box::pin(async { unreachable!("fixture wallets never sign") })
    }
}

impl wallets::SingleSender for FixtureWallet {
    /// Records the transfer and its one submission.
    fn send<'a>(
        &'a self,
        _destination: Address,
        amount: Decimal,
    ) -> wallets::FutureResult<'a, TransactionId> {
        Box::pin(async move {
            self.calls
                .transfers
                .lock()
                .expect("fixture transfer lock")
                .push(amount);
            *self
                .calls
                .broadcasts
                .lock()
                .expect("fixture broadcast lock") += 1;
            Ok(TransactionId::new("fixture-single"))
        })
    }
}

impl wallets::TransactionFactory for FixtureWallet {
    fn transaction(&self) -> Box<dyn base::TransactionBuilder> {
        unreachable!("fixture wallets build no transactions")
    }

    fn resume(
        &self,
        _snapshot: &base::TransactionSnapshot,
    ) -> Result<Box<dyn base::TransactionBuilder>, base::TransactionError> {
        unreachable!("fixture wallets restore no transactions")
    }

    fn broadcaster(&self) -> &dyn base::Broadcaster {
        unreachable!("fixture wallets broadcast nothing")
    }
}

struct Fixture {
    app: Router,
    ready: watch::Sender<SyncHealth>,
    calls: Arc<Calls>,
    resumed: Arc<FixtureResume>,
}

/// The supervisor handle the status route resumes through, recording every
/// network it was asked for so a test can prove the route reached it.
#[derive(Default)]
struct FixtureResume {
    networks: Mutex<Vec<Network>>,
}

impl payment_api::Resume for FixtureResume {
    fn resume(&self, network: &Network) -> Result<(), IndexError> {
        self.networks
            .lock()
            .expect("fixture resume list")
            .push(network.clone());
        Ok(())
    }
}

impl FixtureResume {
    fn networks(&self) -> Vec<Network> {
        self.networks.lock().expect("fixture resume list").clone()
    }
}

/// Every fixture network in one state, as the supervisor's monitor would
/// publish it; `Quarantined` also carries the failure the routes report.
fn health(state: NetworkState) -> SyncHealth {
    let networks = [network(), solana_network(), ethereum_network()];
    let mut health = SyncHealth::starting(&networks);
    for network in &networks {
        health.apply(&reported(network, state));
    }
    health
}

/// Every fixture network ready, with the first one owing a range its node
/// will never serve.
fn stalled() -> SyncHealth {
    let networks = [network(), solana_network(), ethereum_network()];
    let mut health = SyncHealth::starting(&networks);
    for each in &networks {
        let mut row = reported(each, NetworkState::Ready);
        if *each == network() {
            row.backfill = Some(indexing::BackfillStatus {
                pending: 1,
                resume: BlockPosition(2),
                anchor: BlockPosition(8),
                inspected: 0,
                failure: Some(IndexError::new(
                    IndexErrorKind::Source,
                    "block is not available on this node",
                    true,
                )),
            });
        }
        health.apply(&row);
    }
    health
}

fn reported(network: &Network, state: NetworkState) -> NetworkHealth {
    NetworkHealth {
        state,
        failure: (state == NetworkState::Quarantined).then(|| {
            IndexError::new(
                IndexErrorKind::InvalidBlock,
                "block contains a duplicate transaction",
                false,
            )
        }),
        attempts: u32::from(state == NetworkState::Quarantined),
        since: 1_758_614_400,
        ..NetworkHealth::starting(network)
    }
}

fn fixture(initially_ready: bool) -> Fixture {
    fixture_with_usdc(initially_ready, true)
}

fn fixture_with_usdc(initially_ready: bool, usdc: bool) -> Fixture {
    fixture_with_generation_error(initially_ready, usdc, None)
}

fn fixture_with_generation_error(
    initially_ready: bool,
    usdc: bool,
    generation_error: Option<wallets::WalletErrorKind>,
) -> Fixture {
    let calls = Arc::new(Calls::default());
    let Composed {
        index,
        history,
        balance,
    } = FixtureIndex::composed(vec![network(), solana_network(), ethereum_network()]);
    let mut wallets = Wallets::<AssetTag>::new();
    wallets
        .add(
            AssetTag::from_static("btc"),
            FixtureFactory {
                network: network(),
                calls: Arc::clone(&calls),
                generation_error,
            },
        )
        .expect("fixture asset must register");
    wallets
        .add(
            AssetTag::from_static("sol"),
            FixtureFactory {
                network: solana_network(),
                calls: Arc::clone(&calls),
                generation_error: None,
            },
        )
        .expect("native SOL fixture must register");
    if usdc {
        wallets
            .add(
                AssetTag::from_static("usdc"),
                FixtureFactory {
                    network: ethereum_network(),
                    calls: Arc::clone(&calls),
                    generation_error: None,
                },
            )
            .expect("USDC fixture must register");
    }
    let (ready, receiver) = watch::channel(health(if initially_ready {
        NetworkState::Ready
    } else {
        NetworkState::CatchingUp
    }));
    let resumed = Arc::new(FixtureResume::default());
    let payments = Payments::from_params(payment_api::PaymentsParams {
        wallets: Arc::new(wallets),
        units: Arc::new(units()),
        signers: Arc::new(Signers::new()),
        index,
        history,
        balance,
        resume: Arc::clone(&resumed) as Arc<dyn payment_api::Resume>,
        health: receiver,
    });
    let token = http_server::BearerToken::new(TOKEN).expect("fixture bearer token");
    let config = http_server::Config::new(
        "127.0.0.1:0".parse().expect("fixture bind address"),
        http_server::TransportSecurity::PlaintextLoopback,
        Some(token),
        http_server::RequestLimits::default(),
    );
    Fixture {
        app: router(payments, &config).expect("fixture router must build"),
        ready,
        calls,
        resumed,
    }
}

#[tokio::test]
async fn unconfigured_wallet_asset_is_not_found() {
    let fixture = fixture_with_usdc(true, false);
    let response = request(
        &fixture.app,
        "POST",
        "/v1/wallets",
        Some(json!({"asset": "usdc", "start": "fresh"})),
        true,
    )
    .await;

    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(
        json_body(&response)["message"],
        "wallet asset is not configured"
    );
}

#[tokio::test]
async fn wallet_generation_errors_preserve_status_and_message() {
    for (kind, status, message) in [
        (
            wallets::WalletErrorKind::Unsupported,
            StatusCode::NOT_FOUND,
            "wallet asset is not configured",
        ),
        (
            wallets::WalletErrorKind::Generation,
            StatusCode::SERVICE_UNAVAILABLE,
            "fixture wallet generation failed",
        ),
        (
            wallets::WalletErrorKind::Conflict,
            StatusCode::CONFLICT,
            "fixture wallet generation failed",
        ),
        (
            wallets::WalletErrorKind::InvalidAddress,
            StatusCode::BAD_REQUEST,
            "fixture wallet generation failed",
        ),
        (
            wallets::WalletErrorKind::NotFound,
            StatusCode::NOT_FOUND,
            "fixture wallet generation failed",
        ),
    ] {
        let fixture = fixture_with_generation_error(true, false, Some(kind));
        let response = request(
            &fixture.app,
            "POST",
            "/v1/wallets",
            Some(json!({"asset": "btc", "start": "fresh"})),
            true,
        )
        .await;

        assert_eq!(response.status, status, "{kind:?}");
        assert_eq!(
            json_body(&response),
            json!({"message": message}),
            "{kind:?}"
        );
        assert_no_transaction_calls(&fixture.calls);
    }
}

#[tokio::test]
async fn invalid_wallet_asset_uses_the_json_error_contract() {
    let fixture = fixture(true);
    for (asset, status, message) in [
        (
            "USD",
            StatusCode::BAD_REQUEST,
            "request body must match the documented JSON schema",
        ),
        (
            "usd",
            StatusCode::NOT_FOUND,
            "wallet asset is not configured",
        ),
    ] {
        let response = request(
            &fixture.app,
            "POST",
            "/v1/wallets",
            Some(json!({"asset": asset, "start": "fresh"})),
            true,
        )
        .await;
        assert_eq!(response.status, status, "{asset}");
        assert_eq!(json_body(&response)["message"], message, "{asset}");
    }
}

#[tokio::test]
async fn readiness_reflects_runtime_state_while_liveness_stays_available() {
    let fixture = fixture(false);

    assert_eq!(
        request(&fixture.app, "GET", "/health/live", None, false)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(&fixture.app, "GET", "/health/ready", None, false)
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );

    fixture
        .ready
        .send(health(NetworkState::Ready))
        .expect("readiness receiver exists");

    assert_eq!(
        request(&fixture.app, "GET", "/health/ready", None, false)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn one_quarantined_network_keeps_the_others_ready_and_reports_its_failure() {
    let fixture = fixture(true);
    let mut quarantined = health(NetworkState::Ready);
    quarantined.apply(&reported(&solana_network(), NetworkState::Quarantined));
    fixture
        .ready
        .send(quarantined)
        .expect("readiness receiver exists");

    assert_eq!(
        request(&fixture.app, "GET", "/health/ready", None, false)
            .await
            .status,
        StatusCode::NO_CONTENT,
        "a quarantined network must not hold readiness at 503 forever"
    );
    let status = json_body(&request(&fixture.app, "GET", "/v1/status", None, true).await);
    assert_eq!(status["ready"], true);
    let networks = status["networks"].as_array().expect("networks");
    assert_eq!(networks[0]["state"], "ready");
    assert_eq!(networks[0]["failure"], Value::Null);
    assert_eq!(networks[1]["network"]["chain"], "solana");
    assert_eq!(networks[1]["state"], "quarantined");
    assert_eq!(
        networks[1]["failure"],
        json!({
            "kind": "invalid_block",
            "retryable": false,
            "attempts": 1,
            "since": 1_758_614_400_u64,
            "message": "block contains a duplicate transaction",
        }),
        "a quarantined network still reports its frozen checkpoint and why"
    );
    assert_eq!(networks[1]["checkpoint"]["height"], 8);

    fixture
        .ready
        .send(health(NetworkState::Quarantined))
        .expect("readiness receiver exists");
    assert_eq!(
        request(&fixture.app, "GET", "/health/ready", None, false)
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE,
        "every network quarantined serves nothing useful"
    );
}

#[tokio::test]
async fn closed_readiness_is_unavailable_even_when_the_last_value_was_true() {
    for last_value in [false, true] {
        let fixture = fixture(last_value);
        drop(fixture.ready);

        let response = request(&fixture.app, "GET", "/health/ready", None, false).await;
        assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(response.body.is_empty());
        assert_eq!(
            request(&fixture.app, "GET", "/health/live", None, false)
                .await
                .status,
            StatusCode::NO_CONTENT
        );
    }
}

#[tokio::test]
async fn openapi_serves_the_merged_contract_without_authentication() {
    let fixture = fixture(false);
    let response = request(&fixture.app, "GET", "/openapi.json", None, false).await;

    assert_eq!(response.status, StatusCode::OK);
    let document = json_body(&response);
    for path in ["/openapi.json", "/health/ready", "/v1/wallets/{id}"] {
        assert!(document["paths"][path]["get"].is_object(), "{path}");
    }
    assert!(document["components"]["schemas"]["Wallet"].is_object());
    assert_no_transaction_calls(&fixture.calls);
}

#[tokio::test]
async fn missing_wallet_read_preserves_the_not_found_error_contract() {
    let fixture = fixture(true);
    let response = request(
        &fixture.app,
        "GET",
        "/v1/wallets/missing-wallet",
        None,
        true,
    )
    .await;

    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(
        json_body(&response),
        json!({"message": "wallet does not exist"})
    );
    assert_no_transaction_calls(&fixture.calls);
}

#[tokio::test]
async fn wallet_routes_delegate_to_the_wallet_collection() {
    let fixture = fixture(true);
    let unauthorized = request(
        &fixture.app,
        "POST",
        "/v1/wallets",
        Some(json!({"asset": "btc", "start": "fresh"})),
        false,
    )
    .await;
    assert_eq!(unauthorized.status, StatusCode::UNAUTHORIZED);

    let created = request(
        &fixture.app,
        "POST",
        "/v1/wallets",
        Some(json!({"asset": "btc", "start": "fresh"})),
        true,
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let wallet: Value = serde_json::from_slice(&created.body).expect("wallet JSON");
    let id = wallet["id"].as_str().expect("wallet ID");
    assert_eq!(wallet["asset"], "btc");
    assert_eq!(
        wallet["network"],
        json!({"chain": "bitcoin", "name": "regtest"})
    );
    assert_eq!(wallet["address"], "fixture-address");
    assert_eq!(wallet["watch_only"], false);

    let usdc = request(
        &fixture.app,
        "POST",
        "/v1/wallets",
        Some(json!({"asset": "usdc", "start": "fresh"})),
        true,
    )
    .await;
    assert_eq!(usdc.status, StatusCode::CREATED);
    let usdc = json_body(&usdc);
    assert_eq!(usdc["asset"], "usdc");
    assert_eq!(
        usdc["network"],
        json!({"chain": "ethereum", "name": "mainnet"})
    );

    let sol = request(
        &fixture.app,
        "POST",
        "/v1/wallets",
        Some(json!({"asset": "sol", "start": "fresh"})),
        true,
    )
    .await;
    assert_eq!(sol.status, StatusCode::CREATED);
    let sol = json_body(&sol);
    assert_eq!(sol["asset"], "sol");
    assert_eq!(
        sol["network"],
        json!({"chain": "solana", "name": "localnet"})
    );

    let read = request(
        &fixture.app,
        "GET",
        &format!("/v1/wallets/{id}"),
        None,
        true,
    )
    .await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<Value>(&read.body).expect("wallet JSON"),
        wallet
    );

    let balance = request(
        &fixture.app,
        "GET",
        &format!("/v1/wallets/{id}/balance"),
        None,
        true,
    )
    .await;
    assert_eq!(balance.status, StatusCode::OK);
    assert_eq!(
        json_body(&balance),
        json!({
            "amount": "12.5",
            "pending_incoming": "0.25",
            "pending_outgoing": "0",
            "observed_height": 8,
            "pending_height": 9,
            "completeness": "incomplete"
        }),
        "both halves are the index's own fold, scaled by the asset's decimals, \
         and the fold says how much of the history behind it it can prove"
    );

    let history = request(
        &fixture.app,
        "GET",
        &format!("/v1/wallets/{id}/transactions?limit=10"),
        None,
        true,
    )
    .await;
    assert_eq!(history.status, StatusCode::OK);
    let history = json_body(&history);
    assert_eq!(history["checkpoint"]["height"], 8);
    assert_eq!(
        history["transactions"][0]["transaction_id"],
        "fixture-observed"
    );
    assert_eq!(history["transactions"][0]["status"]["kind"], "confirmed");
    assert_eq!(history["transactions"][0]["status"]["confirmations"], 2);
    assert_eq!(history["pending"][0]["transaction_id"], "fixture-pending");
    assert_eq!(history["pending"][0]["status"]["kind"], "pending");
    assert_eq!(history["pending"][0]["status"]["block"]["height"], 9);
    assert_eq!(
        history["completeness"], "incomplete",
        "a page states its completeness in the same words the balance does"
    );
    assert_eq!(history["transactions"][0]["movements"][0]["amount"], "1.5");

    let submitted = request(
        &fixture.app,
        "POST",
        &format!("/v1/wallets/{id}/transactions"),
        Some(json!({
            "destination": "FIXTURE-DESTINATION",
            "amount": "2.25"
        })),
        true,
    )
    .await;
    assert_eq!(submitted.status, StatusCode::ACCEPTED);
    assert_eq!(
        json_body(&submitted),
        json!({"transaction_id": "fixture-single"})
    );

    let batch = request(
        &fixture.app,
        "POST",
        "/v1/transactions",
        Some(json!({"transfers": [{
            "wallet_id": id,
            "destination": "fixture-destination",
            "amount": "3"
        }]})),
        true,
    )
    .await;
    assert_eq!(batch.status, StatusCode::ACCEPTED);
    assert_eq!(
        json_body(&batch),
        json!({"transaction_ids": ["fixture-batch"]})
    );

    assert_eq!(
        fixture
            .calls
            .transfers
            .lock()
            .expect("transfer calls")
            .as_slice(),
        &["2.25".parse::<Decimal>().expect("fixed decimal")]
    );
    assert_eq!(
        *fixture.calls.broadcasts.lock().expect("broadcast calls"),
        1
    );
    assert_eq!(
        fixture
            .calls
            .batches
            .lock()
            .expect("batch calls")
            .as_slice(),
        &[1]
    );
}

#[tokio::test]
async fn transaction_query_rejection_follows_auth_and_precedes_body_semantics() {
    let fixture = fixture(true);
    let unauthorized = raw_request(
        &fixture.app,
        "POST",
        "/v1/transactions?commitment=finalized",
        Some("{"),
        false,
        &[],
    )
    .await;
    assert_eq!(unauthorized.status, StatusCode::UNAUTHORIZED);

    let wallet_id = generated_wallet_id(&fixture).await;
    let malformed = raw_request(
        &fixture.app,
        "POST",
        &format!("/v1/wallets/{wallet_id}/transactions?commitment=finalized"),
        Some("{"),
        true,
        &[],
    )
    .await;
    assert_transaction_query_rejected(&malformed);

    let empty = request(
        &fixture.app,
        "POST",
        "/v1/transactions?min_context_slot=9",
        Some(json!({"transfers": []})),
        true,
    )
    .await;
    assert_transaction_query_rejected(&empty);

    let transfers = (0..51)
        .map(|_| {
            json!({
                "wallet_id": wallet_id,
                "destination": "fixture-destination",
                "amount": "1"
            })
        })
        .collect::<Vec<_>>();
    let oversized = request(
        &fixture.app,
        "POST",
        "/v1/transactions?priority_fee=1",
        Some(json!({"transfers": transfers})),
        true,
    )
    .await;
    assert_transaction_query_rejected(&oversized);
    assert_no_transaction_calls(&fixture.calls);
}

#[tokio::test]
async fn empty_query_and_unrecognized_headers_do_not_change_transaction_semantics() {
    let fixture = fixture(true);
    let wallet_id = generated_wallet_id(&fixture).await;
    let single_body = json!({
        "destination": "fixture-destination",
        "amount": "2.25"
    })
    .to_string();
    let single = raw_request(
        &fixture.app,
        "POST",
        &format!("/v1/wallets/{wallet_id}/transactions?"),
        Some(&single_body),
        true,
        &[
            ("x-min-context-slot", "9"),
            ("traceparent", "fixture-trace"),
        ],
    )
    .await;
    assert_eq!(single.status, StatusCode::ACCEPTED);
    assert_eq!(
        json_body(&single),
        json!({"transaction_id": "fixture-single"})
    );

    let batch_body = json!({"transfers": [{
        "wallet_id": wallet_id,
        "destination": "fixture-destination",
        "amount": "3"
    }]})
    .to_string();
    let batch = raw_request(
        &fixture.app,
        "POST",
        "/v1/transactions?",
        Some(&batch_body),
        true,
        &[("x-retry-transaction", "true")],
    )
    .await;
    assert_eq!(batch.status, StatusCode::ACCEPTED);
    assert_eq!(
        json_body(&batch),
        json!({"transaction_ids": ["fixture-batch"]})
    );

    assert_eq!(
        fixture
            .calls
            .transfers
            .lock()
            .expect("transfer calls")
            .as_slice(),
        &["2.25".parse::<Decimal>().expect("fixed decimal")]
    );
    assert_eq!(
        *fixture.calls.broadcasts.lock().expect("broadcast calls"),
        1
    );
    assert_eq!(
        fixture
            .calls
            .batches
            .lock()
            .expect("batch calls")
            .as_slice(),
        &[1]
    );
}

#[tokio::test]
async fn batch_wire_maximum_precedes_conversion_and_leaves_minimum_to_the_sdk() {
    let fixture = fixture(true);

    let empty = request(
        &fixture.app,
        "POST",
        "/v1/transactions",
        Some(json!({"transfers": []})),
        true,
    )
    .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(&empty),
        json!({"message": "at least one transfer is required"})
    );

    let transfers = (0..=wallets::MAX_TRANSFERS)
        .map(|_| {
            json!({
                "wallet_id": "missing-wallet",
                "destination": "fixture-destination",
                "amount": "not-a-decimal"
            })
        })
        .collect::<Vec<_>>();
    let oversized = request(
        &fixture.app,
        "POST",
        "/v1/transactions",
        Some(json!({
            "transfers": transfers
        })),
        true,
    )
    .await;
    assert_eq!(oversized.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(&oversized),
        json!({"message": "at most 50 transfers are allowed"})
    );
    assert_no_transaction_calls(&fixture.calls);

    let wallet_id = generated_wallet_id(&fixture).await;
    for admitted_count in [1, wallets::MAX_TRANSFERS] {
        let destination = json!("fixture-destination");
        let transfers = (0..admitted_count)
            .map(|_| {
                json!({
                    "wallet_id": wallet_id,
                    "destination": destination,
                    "amount": "1"
                })
            })
            .collect::<Vec<_>>();
        let admitted = request(
            &fixture.app,
            "POST",
            "/v1/transactions",
            Some(json!({"transfers": transfers})),
            true,
        )
        .await;
        assert_eq!(admitted.status, StatusCode::ACCEPTED);
        assert_eq!(
            json_body(&admitted),
            json!({"transaction_ids": ["fixture-batch"]})
        );
    }

    assert_eq!(
        fixture
            .calls
            .batches
            .lock()
            .expect("batch calls")
            .as_slice(),
        &[1, wallets::MAX_TRANSFERS]
    );
    assert!(
        fixture
            .calls
            .transfers
            .lock()
            .expect("transfer calls")
            .is_empty()
    );
    assert_eq!(
        *fixture.calls.broadcasts.lock().expect("broadcast calls"),
        0
    );
}

#[tokio::test]
async fn transaction_bodies_reject_unknown_controls_before_sdk_delegation() {
    let fixture = fixture(true);
    let wallet_id = generated_wallet_id(&fixture).await;
    let single_path = format!("/v1/wallets/{wallet_id}/transactions");
    let priority_body = json!({
        "transfers": [{
            "wallet_id": wallet_id,
            "destination": "fixture-destination",
            "amount": "1"
        }],
        "priority_fee": "1"
    });
    let cases = [
        (
            "single destination lag control",
            single_path.as_str(),
            json!({
                "destination": {
                    "encoding": "hex",
                    "text": "fixture-destination",
                    "max_lag": 4
                },
                "amount": "1"
            }),
        ),
        (
            "batch destination reference control",
            "/v1/transactions",
            json!({"transfers": [{
                "wallet_id": wallet_id,
                "destination": {
                    "encoding": "hex",
                    "text": "fixture-destination",
                    "reference_slot": 9
                },
                "amount": "1"
            }]}),
        ),
        (
            "single commitment control",
            single_path.as_str(),
            json!({
                "destination": "fixture-destination",
                "amount": "1",
                "commitment": "finalized"
            }),
        ),
        (
            "single retry control",
            single_path.as_str(),
            json!({
                "destination": "fixture-destination",
                "amount": "1",
                "retry": true
            }),
        ),
        (
            "batch item Memo override",
            "/v1/transactions",
            json!({"transfers": [{
                "wallet_id": wallet_id,
                "destination": "fixture-destination",
                "amount": "1",
                "memo": "caller-selected"
            }]}),
        ),
        ("batch priority control", "/v1/transactions", priority_body),
    ];

    for (case, path, body) in cases {
        let response = request(&fixture.app, "POST", path, Some(body), true).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{case}");
        assert_eq!(
            json_body(&response),
            json!({"message": "request body must match the documented JSON schema"}),
            "{case}"
        );
        assert_no_transaction_calls(&fixture.calls);
    }
}

#[tokio::test]
async fn destinations_are_parsed_by_the_wallet_chain_before_any_send() {
    let fixture = fixture(true);
    let wallet_id = generated_wallet_id(&fixture).await;
    let single = request(
        &fixture.app,
        "POST",
        &format!("/v1/wallets/{wallet_id}/transactions"),
        Some(json!({"destination": "not-an-address", "amount": "1"})),
        true,
    )
    .await;
    assert_eq!(single.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(&single),
        json!({"message": "fixture destination is invalid"})
    );

    let batch = request(
        &fixture.app,
        "POST",
        "/v1/transactions",
        Some(json!({"transfers": [
            {"wallet_id": wallet_id, "destination": "fixture-destination", "amount": "1"},
            {"wallet_id": wallet_id, "destination": "not-an-address", "amount": "1"}
        ]})),
        true,
    )
    .await;
    assert_eq!(batch.status, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&batch)["failed_index"], 1);
    assert_no_transaction_calls(&fixture.calls);
}

#[tokio::test]
async fn assets_are_listed_from_the_registered_factories() {
    let fixture = fixture(true);
    let response = request(&fixture.app, "GET", "/v1/assets", None, true).await;
    assert_eq!(response.status, StatusCode::OK);
    let assets = json_body(&response)["assets"].clone();
    let summary = assets
        .as_array()
        .expect("asset list")
        .iter()
        .map(|asset| {
            format!(
                "{}:{}:{}:{}",
                asset["asset"].as_str().expect("tag"),
                asset["network"]["chain"].as_str().expect("chain"),
                asset["network"]["name"].as_str().expect("network name"),
                asset["metadata"]["decimals"]
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        summary,
        [
            "btc:bitcoin:regtest:8",
            "sol:solana:localnet:8",
            "usdc:ethereum:mainnet:8"
        ]
    );
    assert_eq!(
        request(&fixture.app, "GET", "/v1/assets", None, false)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn address_text_is_parsed_by_the_asset_without_a_wallet() {
    let fixture = fixture(true);
    let parsed = request(
        &fixture.app,
        "GET",
        "/v1/assets/sol/address?text=FIXTURE-Watched",
        None,
        true,
    )
    .await;
    assert_eq!(parsed.status, StatusCode::OK);
    assert_eq!(
        json_body(&parsed),
        json!({
            "network": {"chain": "solana", "name": "localnet"},
            "value": "fixture-watched"
        })
    );
    for (path, status) in [
        ("/v1/assets/sol/address?text=nope", StatusCode::BAD_REQUEST),
        (
            "/v1/assets/doge/address?text=fixture-a",
            StatusCode::NOT_FOUND,
        ),
        (
            "/v1/assets/DOGE/address?text=fixture-a",
            StatusCode::BAD_REQUEST,
        ),
    ] {
        assert_eq!(
            request(&fixture.app, "GET", path, None, true).await.status,
            status,
            "{path}"
        );
    }
}

#[tokio::test]
async fn imported_wallets_are_parsed_by_the_asset_and_never_echo_the_secret() {
    let fixture = fixture(true);
    let imported = request(
        &fixture.app,
        "POST",
        "/v1/wallets/import",
        Some(json!({"asset": "usdc", "secret": "2a", "start": "fresh"})),
        true,
    )
    .await;
    assert_eq!(imported.status, StatusCode::CREATED);
    let wallet = json_body(&imported);
    assert_eq!(wallet["asset"], "usdc");
    assert_eq!(wallet["network"]["chain"], "ethereum");
    assert_eq!(wallet["address"], "fixture-2a");
    assert_eq!(wallet["watch_only"], false);
    assert!(!String::from_utf8_lossy(&imported.body).contains("\"2a\""));

    for (body, status, message) in [
        (
            json!({"asset": "usdc", "secret": "secret-text", "start": "fresh"}),
            StatusCode::BAD_REQUEST,
            "wallet secret must be hexadecimal key material",
        ),
        (
            json!({"asset": "doge", "secret": "2a", "start": "fresh"}),
            StatusCode::NOT_FOUND,
            "wallet asset is not configured",
        ),
        (
            json!({"asset": "usdc", "secret": "2b", "start_position": 1}),
            StatusCode::BAD_REQUEST,
            "request body must match the documented JSON schema",
        ),
    ] {
        let response = request(&fixture.app, "POST", "/v1/wallets/import", Some(body), true).await;
        assert_eq!(response.status, status);
        assert_eq!(json_body(&response), json!({"message": message}));
        assert!(!String::from_utf8_lossy(&response.body).contains("secret-text"));
    }
    // The index owns the watch list, so one address is watched once whatever
    // id a second import would mint — which is the 409 the contract has always
    // documented, now answered the same way with and without a registry.
    let duplicate = request(
        &fixture.app,
        "POST",
        "/v1/wallets/import",
        Some(json!({"asset": "usdc", "secret": "2a", "start": "fresh"})),
        true,
    )
    .await;
    assert_eq!(
        duplicate.status,
        StatusCode::CONFLICT,
        "one address is watched once"
    );
    assert_no_transaction_calls(&fixture.calls);
}

#[tokio::test]
async fn watched_addresses_have_history_and_balance_but_no_sends() {
    let fixture = fixture(true);
    let watched = request(
        &fixture.app,
        "POST",
        "/v1/addresses",
        Some(json!({"asset": "btc", "address": "FIXTURE-Watched", "start": "fresh"})),
        true,
    )
    .await;
    assert_eq!(watched.status, StatusCode::CREATED);
    let wallet = json_body(&watched);
    let id = wallet["id"].as_str().expect("watch ID").to_owned();
    assert_eq!(wallet["address"], "fixture-watched");
    assert_eq!(wallet["watch_only"], true);

    let history = request(
        &fixture.app,
        "GET",
        &format!("/v1/wallets/{id}/transactions"),
        None,
        true,
    )
    .await;
    assert_eq!(history.status, StatusCode::OK);
    let movement = &json_body(&history)["transactions"][0]["movements"][0];
    assert_eq!(movement["to"]["value"], "fixture-watched");
    assert_eq!(movement["amount"], "1.5");

    let balance = request(
        &fixture.app,
        "GET",
        &format!("/v1/wallets/{id}/balance"),
        None,
        true,
    )
    .await;
    assert_eq!(
        balance.status,
        StatusCode::OK,
        "a fold over indexed movements needs no key"
    );
    assert_eq!(json_body(&balance)["amount"], "12.5");
    let send = request(
        &fixture.app,
        "POST",
        &format!("/v1/wallets/{id}/transactions"),
        Some(json!({"destination": "fixture-destination", "amount": "1"})),
        true,
    )
    .await;
    assert_eq!(send.status, StatusCode::UNPROCESSABLE_ENTITY);
    let batch = request(
        &fixture.app,
        "POST",
        "/v1/transactions",
        Some(json!({"transfers": [
            {"wallet_id": id, "destination": "fixture-destination", "amount": "1"}
        ]})),
        true,
    )
    .await;
    assert_eq!(batch.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json_body(&batch)["failed_index"], 0);
    assert_no_transaction_calls(&fixture.calls);

    for (body, status) in [
        (
            json!({"asset": "btc", "address": "not-an-address", "start": "fresh"}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"asset": "doge", "address": "fixture-a", "start": "fresh"}),
            StatusCode::NOT_FOUND,
        ),
    ] {
        let response = request(&fixture.app, "POST", "/v1/addresses", Some(body), true).await;
        assert_eq!(response.status, status);
    }
}

#[tokio::test]
async fn wallet_list_holds_generated_imported_and_watched_entries() {
    let fixture = fixture(true);
    let generated = generated_wallet_id(&fixture).await;
    let empty = request(&fixture.app, "GET", "/v1/wallets", None, true).await;
    assert_eq!(empty.status, StatusCode::OK);
    assert_eq!(
        json_body(&empty)["wallets"].as_array().map(Vec::len),
        Some(1)
    );
    request(
        &fixture.app,
        "POST",
        "/v1/addresses",
        Some(json!({"asset": "sol", "address": "fixture-watched", "start": "fresh"})),
        true,
    )
    .await;
    let listed = json_body(&request(&fixture.app, "GET", "/v1/wallets", None, true).await);
    let wallets = listed["wallets"].as_array().expect("wallet list");
    assert_eq!(wallets.len(), 2);
    assert_eq!(
        wallets[0]["id"],
        generated.as_str(),
        "UUIDv7 IDs list in creation order"
    );
    assert_eq!(wallets[1]["watch_only"], true);
    assert_eq!(wallets[1]["network"]["chain"], "solana");
}

#[tokio::test]
async fn status_reports_readiness_and_every_indexed_checkpoint() {
    let fixture = fixture(false);
    let response = request(&fixture.app, "GET", "/v1/status", None, true).await;
    assert_eq!(response.status, StatusCode::OK);
    let status = json_body(&response);
    assert_eq!(status["ready"], false);
    let networks = status["networks"].as_array().expect("networks");
    assert_eq!(networks.len(), 3);
    assert_eq!(networks[0]["network"]["chain"], "bitcoin");
    assert_eq!(networks[0]["checkpoint"]["height"], 8);
    assert_eq!(networks[0]["state"], "catching_up");
    assert_eq!(networks[0]["failure"], Value::Null);
    fixture
        .ready
        .send(health(NetworkState::Ready))
        .expect("readiness receiver exists");
    let status = json_body(&request(&fixture.app, "GET", "/v1/status", None, true).await);
    assert_eq!(status["ready"], true);
}

/// A birthday the caller states reaches the index verbatim, and one the index
/// will not read is refused by the call that made it.
///
/// This is the whole of what keeps a caller-supplied `start` from wedging
/// readiness: the range a watch may owe is bounded synchronously, under the
/// permit that fixed the checkpoint it is relative to, so no background pass
/// inherits a request that can never finish (`docs/design/024` section 7).
#[tokio::test]
async fn a_stated_birthday_reaches_the_index_and_an_unreadable_one_is_refused() {
    let fixture = fixture(true);

    for (route, body) in [
        ("/v1/wallets", json!({"asset": "btc", "start": 4})),
        (
            "/v1/wallets/import",
            json!({"asset": "usdc", "secret": "2a", "start": 4}),
        ),
        (
            "/v1/addresses",
            json!({"asset": "sol", "address": "fixture-early", "start": 4}),
        ),
    ] {
        let response = request(&fixture.app, "POST", route, Some(body), true).await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "{route} must refuse a start the index will not read"
        );
        assert_eq!(
            json_body(&response)["message"],
            "the earliest acceptable start is 5",
            "{route} passes the index's own refusal through, naming a start that works"
        );
    }

    let accepted = request(
        &fixture.app,
        "POST",
        "/v1/wallets",
        Some(json!({"asset": "btc", "start": EARLIEST_START})),
        true,
    )
    .await;
    assert_eq!(accepted.status, StatusCode::CREATED);
}

/// An omitted birthday is a parse failure, not a default: the decision that
/// every add path states one is enforced by the parser.
#[tokio::test]
async fn an_add_path_without_a_start_is_refused_by_the_parser() {
    let fixture = fixture(true);

    for (route, body) in [
        ("/v1/wallets", json!({"asset": "btc"})),
        (
            "/v1/wallets/import",
            json!({"asset": "usdc", "secret": "2a"}),
        ),
        (
            "/v1/addresses",
            json!({"asset": "sol", "address": "fixture-late"}),
        ),
        ("/v1/wallets", json!({"asset": "btc", "start": "tip"})),
    ] {
        let response = request(&fixture.app, "POST", route, Some(body), true).await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "{route} must not invent a birthday"
        );
    }
}

/// The availability floor at the HTTP edge: a range this node can never serve
/// is published on the status row and does not make the API unready.
#[tokio::test]
async fn a_network_that_owes_an_unservable_range_is_still_ready_and_says_so() {
    let fixture = fixture(false);
    fixture
        .ready
        .send(stalled())
        .expect("readiness receiver exists");

    let status = json_body(&request(&fixture.app, "GET", "/v1/status", None, true).await);

    assert_eq!(
        status["ready"], true,
        "forward indexing reached every tip, so the API answers"
    );
    let stuck = &status["networks"][0];
    assert_eq!(stuck["state"], "ready");
    assert_eq!(stuck["failure"], Value::Null, "nothing holds the tip back");
    assert_eq!(stuck["backfill"]["pending"], 1);
    assert_eq!(stuck["backfill"]["resume"], 2);
    assert_eq!(stuck["backfill"]["anchor"], 8);
    assert_eq!(stuck["backfill"]["failure"]["kind"], "source");
    assert_eq!(
        stuck["backfill"]["failure"]["message"], "block is not available on this node",
        "an operator reads the stuck range here rather than inferring it from a 503"
    );
    assert_eq!(
        status["networks"][1]["backfill"],
        Value::Null,
        "a network that owes nothing says so"
    );

    let probe = request(
        &fixture.app,
        "GET",
        http_server::READINESS_PATH,
        None,
        false,
    )
    .await;
    assert_eq!(probe.status, StatusCode::NO_CONTENT);
}

/// The operator's way back from a quarantine, which before this route existed
/// was restarting the process (`docs/design/019` section 5.3).
#[tokio::test]
async fn resume_clears_one_quarantined_network_and_reports_what_it_cleared() {
    let fixture = fixture(false);
    fixture
        .ready
        .send(health(NetworkState::Quarantined))
        .expect("readiness receiver exists");

    let response = request(
        &fixture.app,
        "POST",
        "/v1/status/resume",
        Some(json!({"network": {"chain": "bitcoin", "name": "regtest"}})),
        true,
    )
    .await;

    assert_eq!(response.status, StatusCode::ACCEPTED);
    assert_eq!(
        json_body(&response),
        json!({
            "network": {"chain": "bitcoin", "name": "regtest"},
            "cleared": {
                "kind": "invalid_block",
                "retryable": false,
                "attempts": 1,
                "since": 1_758_614_400,
                "message": "block contains a duplicate transaction"
            }
        })
    );
    // Exactly the one network asked for: resuming every chain blindly is a
    // different capability from resuming the one an operator repaired.
    assert_eq!(fixture.resumed.networks(), vec![network()]);
}

#[tokio::test]
async fn resume_of_an_unconfigured_network_is_not_found() {
    let fixture = fixture(false);
    fixture
        .ready
        .send(health(NetworkState::Quarantined))
        .expect("readiness receiver exists");

    let response = request(
        &fixture.app,
        "POST",
        "/v1/status/resume",
        Some(json!({"network": {"chain": "bitcoin", "name": "mainnet"}})),
        true,
    )
    .await;

    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(
        json_body(&response),
        json!({"message": "index network is not configured"})
    );
    assert!(fixture.resumed.networks().is_empty());
}

/// A resume is an answer to a quarantine, so a network that has none is a
/// conflict rather than an accepted no-op: `202` there would report an action
/// that did not happen.
#[tokio::test]
async fn resume_of_a_network_that_is_not_quarantined_conflicts() {
    for state in [
        NetworkState::Ready,
        NetworkState::CatchingUp,
        NetworkState::Retrying,
    ] {
        let fixture = fixture(false);
        fixture
            .ready
            .send(health(state))
            .expect("readiness receiver exists");

        let response = request(
            &fixture.app,
            "POST",
            "/v1/status/resume",
            Some(json!({"network": {"chain": "bitcoin", "name": "regtest"}})),
            true,
        )
        .await;

        assert_eq!(response.status, StatusCode::CONFLICT, "{state:?}");
        assert_eq!(
            json_body(&response),
            json!({"message": "index network is not quarantined; nothing to resume"}),
            "{state:?}"
        );
        assert!(fixture.resumed.networks().is_empty(), "{state:?}");
    }
}

/// The operator's answer to the range the previous test publishes: stop asking.
///
/// It settles the range and leaves the watch where one created now would begin,
/// which is what stops an address's reads claiming history nobody read. Nothing
/// indexed is removed, and the id is the same one afterwards.
#[tokio::test]
async fn cancelling_a_backfill_settles_the_range_and_keeps_the_watch() {
    let fixture = fixture(true);
    let watched = json_body(
        &request(
            &fixture.app,
            "POST",
            "/v1/addresses",
            Some(json!({"asset": "btc", "address": "fixture-late", "start": 5})),
            true,
        )
        .await,
    );
    let id = watched["id"].as_str().expect("a watched id").to_owned();
    fixture
        .ready
        .send(stalled())
        .expect("readiness receiver exists");

    let response = request(
        &fixture.app,
        "POST",
        "/v1/status/backfill/cancel",
        Some(json!({"network": {"chain": "bitcoin", "name": "regtest"}})),
        true,
    )
    .await;

    assert_eq!(response.status, StatusCode::ACCEPTED);
    assert_eq!(
        json_body(&response),
        json!({
            "network": {"chain": "bitcoin", "name": "regtest"},
            "watches": 1,
            "stopped": {
                "pending": 1,
                "resume": 2,
                "anchor": 8,
                "failure": {
                    "kind": "source",
                    "retryable": true,
                    "attempts": 0,
                    "since": 1_758_614_400,
                    "message": "block is not available on this node"
                }
            }
        }),
        "the response names the range it stopped asking for, as the report held it"
    );
    // The watch is still there, under the same id: cancelling a range is not
    // unwatching an address.
    let wallet = json_body(
        &request(
            &fixture.app,
            "GET",
            &format!("/v1/wallets/{id}"),
            None,
            true,
        )
        .await,
    );
    assert_eq!(wallet["id"], json!(id));
    assert_eq!(wallet["address"], watched["address"]);
}

/// A cancellation is an answer to a range, so a network that owes none is a
/// conflict rather than an accepted no-op, exactly as a resume is.
#[tokio::test]
async fn cancelling_a_backfill_no_network_owes_conflicts() {
    let fixture = fixture(true);

    let response = request(
        &fixture.app,
        "POST",
        "/v1/status/backfill/cancel",
        Some(json!({"network": {"chain": "bitcoin", "name": "regtest"}})),
        true,
    )
    .await;

    assert_eq!(response.status, StatusCode::CONFLICT);
    assert_eq!(
        json_body(&response),
        json!({"message": "index network owes no history; nothing to cancel"})
    );

    let unconfigured = request(
        &fixture.app,
        "POST",
        "/v1/status/backfill/cancel",
        Some(json!({"network": {"chain": "bitcoin", "name": "mainnet"}})),
        true,
    )
    .await;
    assert_eq!(unconfigured.status, StatusCode::NOT_FOUND);

    let unauthorized = request(
        &fixture.app,
        "POST",
        "/v1/status/backfill/cancel",
        Some(json!({"network": {"chain": "bitcoin", "name": "regtest"}})),
        false,
    )
    .await;
    assert_eq!(unauthorized.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn resume_without_a_bearer_token_is_unauthorized() {
    let fixture = fixture(false);
    fixture
        .ready
        .send(health(NetworkState::Quarantined))
        .expect("readiness receiver exists");

    let response = request(
        &fixture.app,
        "POST",
        "/v1/status/resume",
        Some(json!({"network": {"chain": "bitcoin", "name": "regtest"}})),
        false,
    )
    .await;

    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert!(fixture.resumed.networks().is_empty());
}

async fn generated_wallet_id(fixture: &Fixture) -> String {
    let response = request(
        &fixture.app,
        "POST",
        "/v1/wallets",
        Some(json!({"asset": "btc", "start": "fresh"})),
        true,
    )
    .await;
    assert_eq!(response.status, StatusCode::CREATED);
    json_body(&response)["id"]
        .as_str()
        .expect("generated wallet ID")
        .to_owned()
}

fn assert_transaction_query_rejected(response: &Response) {
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(response),
        json!({"message": "transaction query parameters are not supported"})
    );
}

fn assert_no_transaction_calls(calls: &Calls) {
    assert!(calls.transfers.lock().expect("transfer calls").is_empty());
    assert_eq!(*calls.broadcasts.lock().expect("broadcast calls"), 0);
    assert!(calls.batches.lock().expect("batch calls").is_empty());
}

fn network() -> Network {
    Network {
        chain: ChainName::new("bitcoin"),
        name: "regtest".to_owned(),
    }
}

/// What the fixture chains declare: each one's own currency, which is
/// [`COIN`] on all three, as a chain's configuration would.
fn units() -> base::Units {
    [network(), ethereum_network(), solana_network()]
        .into_iter()
        .map(|network| {
            (
                base::AssetId::native(network.chain),
                base::Unit::native(&COIN),
            )
        })
        .collect()
}

fn ethereum_network() -> Network {
    Network {
        chain: ChainName::new("ethereum"),
        name: "mainnet".to_owned(),
    }
}

fn solana_network() -> Network {
    Network {
        chain: ChainName::new("solana"),
        name: "localnet".to_owned(),
    }
}

fn block(height: u64) -> BlockRef {
    BlockRef {
        position: BlockPosition(height),
        height: BlockHeight(height),
        hash: BlockHash(vec![height as u8; 32]),
        parent: None,
        timestamp: Some(height),
    }
}

struct Response {
    status: StatusCode,
    content_type: Option<String>,
    body: Bytes,
}

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    authorized: bool,
) -> Response {
    let body = body.map(|value| value.to_string());
    raw_request(app, method, path, body.as_deref(), authorized, &[]).await
}

async fn raw_request(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<&str>,
    authorized: bool,
    headers: &[(&str, &str)],
) -> Response {
    let mut request = Request::builder().method(method).uri(path);
    if authorized {
        request = request.header("authorization", format!("Bearer {TOKEN}"));
    }
    if body.is_some()
        && !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
    {
        request = request.header("content-type", "application/json");
    }
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(body.unwrap_or_default().to_owned()))
                .expect("fixture request"),
        )
        .await
        .expect("route response");
    let status = response.status();
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("response body")
        .to_bytes();
    Response {
        status,
        content_type,
        body,
    }
}

fn json_body(response: &Response) -> Value {
    serde_json::from_slice(&response.body).expect("response JSON")
}

/// Every rejection is the documented JSON error contract, including the ones
/// Axum and the authentication layer produce before a handler runs. A
/// `text/plain` rejection would break every generated client.
#[tokio::test]
async fn every_rejection_answers_the_json_error_contract() {
    let fixture = fixture(true);
    let id = generated_wallet_id(&fixture).await;
    let history = format!("/v1/wallets/{id}/transactions");

    // 1. An unknown query field on the history route.
    let unknown_field = request(
        &fixture.app,
        "GET",
        &format!("{history}?commitment=finalized"),
        None,
        true,
    )
    .await;
    assert_json_error(&unknown_field, StatusCode::BAD_REQUEST);

    // 2. A limit that is not a number at all.
    let bad_limit = request(
        &fixture.app,
        "GET",
        &format!("{history}?limit=all"),
        None,
        true,
    )
    .await;
    assert_json_error(&bad_limit, StatusCode::BAD_REQUEST);
    // A numeric limit outside the documented range keeps its own message.
    let out_of_range = request(
        &fixture.app,
        "GET",
        &format!("{history}?limit=0"),
        None,
        true,
    )
    .await;
    assert_eq!(
        json_body(&out_of_range),
        json!({"message": "history limit must be between 1 and 1000"})
    );

    // 3. An asset tag that is not a tag, in the path.
    let bad_tag = request(
        &fixture.app,
        "GET",
        "/v1/assets/NOT-A-TAG/address?text=fixture-a",
        None,
        true,
    )
    .await;
    assert_json_error(&bad_tag, StatusCode::BAD_REQUEST);

    // 4. A missing and a wrong bearer token.
    for authorized in [false, true] {
        let response = raw_request(
            &fixture.app,
            "GET",
            "/v1/wallets",
            None,
            false,
            if authorized {
                &[("authorization", "Bearer wrong-secret")]
            } else {
                &[]
            },
        )
        .await;
        assert_json_error(&response, StatusCode::UNAUTHORIZED);
    }

    // 5. A body sent without the JSON content type.
    let wrong_content_type = raw_request(
        &fixture.app,
        "POST",
        "/v1/wallets",
        Some(r#"{"asset":"btc"}"#),
        true,
        &[("content-type", "text/plain")],
    )
    .await;
    assert_json_error(&wrong_content_type, StatusCode::BAD_REQUEST);
}

/// Every error body is `application/json` and carries a non-empty `message`.
fn assert_json_error(response: &Response, status: StatusCode) {
    assert_eq!(response.status, status);
    assert_eq!(
        response
            .content_type
            .as_deref()
            .map(|value| value.split(';').next().unwrap_or_default().trim())
            .unwrap_or_default(),
        "application/json",
        "a {status} rejection must answer JSON, not {:?}",
        response.content_type
    );
    let body = json_body(response);
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "a {status} rejection must carry a message: {body}"
    );
    assert!(
        body.as_object().is_some_and(|body| {
            body.keys().all(|key| {
                [
                    "message",
                    "transaction_ids",
                    "failed_index",
                    "ambiguous_transaction_id",
                ]
                .contains(&key.as_str())
            })
        }),
        "a {status} rejection must use the ErrorBody schema: {body}"
    );
}

/// The layering rule, checked from the inside: this package declares no chain
/// or storage dependency, and no route names one. `linter.toml`'s `api` layer
/// and `scripts/check-chains.sh` check 8 enforce the same boundary.
#[test]
fn the_route_package_names_no_chain_or_storage_crate() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("manifest");
    let dependencies = manifest
        .split_once("[dependencies]")
        .expect("dependency section")
        .1;
    for forbidden in ["chain-", "rusqlite", "sqlite"] {
        assert!(
            !dependencies.contains(forbidden),
            "payment-api must not depend on {forbidden}"
        );
    }
    assert!(
        dependencies
            .contains(r#"indexing = { path = "../../sdk/indexing", default-features = false }"#),
        "payment-api must take indexing without its store or its supervisor"
    );
    let mut sources = vec![root.join("src")];
    let mut checked = 0;
    while let Some(directory) = sources.pop() {
        for entry in std::fs::read_dir(directory).expect("source directory") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                sources.push(path);
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("source file");
            for forbidden in ["chain_bitcoin", "chain_ethereum", "chain_solana"] {
                assert!(
                    !source.contains(forbidden),
                    "{} names {forbidden}",
                    path.display()
                );
            }
            checked += 1;
        }
    }
    assert!(checked > 5, "every route file must be checked");
}
