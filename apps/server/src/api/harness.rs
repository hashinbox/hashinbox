//! What every endpoint's tests run on: the real router over the wallet double
//! (`wallets::testing`) and real indexing of scripted chains
//! (`indexing::testing::Networks`), with health published as plain values, as
//! the supervisor's monitor does.

#![cfg(test)]

use std::sync::Arc;

use axum::{
    body::{Body, Bytes},
    http::{Request, StatusCode, header::CONTENT_TYPE},
};
use http_body_util::BodyExt;
use indexing::{
    ChainName, ChainTips as _, IndexError, IndexErrorKind, Network, NetworkHealth, NetworkState,
    SyncHealth, WalletRegistry as _, Watching as _,
};
use serde_json::Value;
use tokio::sync::watch;
use tower::ServiceExt;
use wallets::{
    Policy,
    testing::{Factory, Outbox, Submission},
};

use super::{
    networks::{LiveNetwork, Rejected, Verifier},
    router,
};
use crate::{
    asset::{AssetTag, Wallets},
    models::{RpcEndpoints, tests::File},
    state::AppState,
};

/// The bearer token every authorized request carries.
pub(super) const TOKEN: &str = "route-test-secret";

/// The tip `App::mine` produces: block 7's payout is confirmed, block 8
/// is the checkpoint, and block 9's payout is pending.
const MINED_TIP: u64 = 9;

/// Every network's tip before anything is mined; a fresh watch is born
/// at the block after it.
const GENESIS_TIP: u64 = 6;

/// What `App::mine` pays every watched address, by block: 1.5 confirmed
/// at block 7, 0.25 still pending at block 9.
const PAYOUTS: [(u64, u64); 2] = [(7, 150_000_000), (9, 25_000_000)];
/// The native coin of every test network.
const COIN: base::Asset = base::Asset::new(
    base::ChainNetwork::new(
        ChainName::from_static("Test"),
        "test",
        base::NetworkKind::Mainnet,
        "TST",
    ),
    "Test Coin",
    "TST",
    8,
);

/// One response, read whole.
pub(super) struct Reply {
    pub(super) status: StatusCode,
    pub(super) content_type: Option<String>,
    pub(super) body: Bytes,
}

impl Reply {
    pub(super) fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("response JSON")
    }
}

/// The router and what it was built over: `btc` on Bitcoin regtest, `sol`
/// on Solana localnet and, unless left out, `usdc` on Ethereum mainnet,
/// each the wallet double's native coin of its network.
pub(super) struct App {
    pub(super) state: AppState,
    ready: Option<watch::Sender<SyncHealth>>,
    router: axum::Router,
    chains: indexing::testing::Networks,
    outboxes: Vec<Outbox>,
    /// The supervisor a rescan pauses, until [`Self::supervised`] runs it.
    supervisor: Option<indexing::Supervisor>,
    stop: watch::Sender<bool>,
    /// The database file the endpoints live in.
    _file: File,
}

impl App {
    /// Every asset registered and every network ready.
    pub(super) async fn ready() -> Self {
        Self::of(Factory::new(Self::bitcoin()), true, NetworkState::Ready).await
    }

    /// Every asset registered and every network still catching up.
    pub(super) async fn catching_up() -> Self {
        Self::of(
            Factory::new(Self::bitcoin()),
            true,
            NetworkState::CatchingUp,
        )
        .await
    }

    /// `btc` made by `bitcoin` and `sol`, with no `usdc`; every network ready.
    pub(super) async fn without_usdc(bitcoin: Factory) -> Self {
        Self::of(bitcoin, false, NetworkState::Ready).await
    }

    async fn of(bitcoin: Factory, usdc: bool, state: NetworkState) -> Self {
        let repository = indexing::memory::Repository::new();
        let chains =
            indexing::testing::Networks::new(&Self::networks(), GENESIS_TIP, 1, repository.clone());
        let (wallets, outboxes) = Self::wallets(bitcoin, usdc);
        let (ready, health) = watch::channel(Self::health(state));
        let file = File::new();
        let (report, _readiness) = indexing::SyncReport::watching(chains.index().networks());
        let supervisor = indexing::Supervisor::new(
            chains.index(),
            indexing::Schedules::every(std::time::Duration::from_secs(3600)),
            report,
        );
        let state = AppState {
            wallets: Arc::new(wallets),
            units: Arc::new(Self::units()),
            signers: Arc::default(),
            pool: file.pool().await,
            registry: Arc::new(repository.clone()),
            index: chains.index(),
            history: chains.history(),
            balance: chains.balance(),
            health,
            networks: Self::networks()
                .map(|network| Self::configured(network, true))
                .into(),
            verify: Self::verifier(),
            control: supervisor.control(),
            purging: Arc::new(repository),
        };
        Self {
            router: Self::route(state.clone()),
            state,
            ready: Some(ready),
            chains,
            outboxes,
            supervisor: Some(supervisor),
            stop: watch::channel(false).0,
            _file: file,
        }
    }

    /// `btc` made by `bitcoin`, `sol`, and `usdc` when asked for, queued as
    /// `main` queues them, so every send route's test goes through the same
    /// queue the server sends through; with each factory's outbox.
    fn wallets(bitcoin: Factory, usdc: bool) -> (Wallets, Vec<Outbox>) {
        let mut factories = vec![("btc", bitcoin), ("sol", Factory::new(Self::solana()))];
        if usdc {
            factories.push(("usdc", Factory::new(Self::ethereum())));
        }
        let mut wallets = Wallets::new();
        let mut outboxes = Vec::new();
        for (tag, factory) in factories {
            outboxes.push(factory.outbox());
            let tag = tag.parse::<AssetTag>().expect("an asset tag");
            wallets.add(tag, factory).expect("one factory per tag");
        }
        wallets.queue(&Policy::default());
        (wallets, outboxes)
    }

    /// One network served by `node.invalid`, or listed and not served.
    fn configured(network: Network, served: bool) -> Arc<LiveNetwork> {
        let nodes = RpcEndpoints::new(vec![serde_json::json!("http://node.invalid:8545")])
            .expect("a usable endpoint");
        let transport = served.then(|| Arc::new(json_rpc::Swap::new(Self::offline(), Vec::new())));
        Arc::new(LiveNetwork::new(network, transport, nodes))
    }

    /// A node nothing in these tests calls.
    fn offline() -> Arc<dyn json_rpc::Client> {
        let node = json_rpc::Endpoint::new("http://node.invalid:8545")
            .connect(&std::collections::BTreeMap::new())
            .expect("a node");
        Arc::new(node)
    }

    /// The chain check of a candidate, in miniature: a node whose host
    /// says `unverified` serves another network.
    fn verifier() -> Verifier {
        Arc::new(|_network, endpoints: RpcEndpoints| {
            Box::pin(async move {
                if endpoints
                    .endpoints()
                    .iter()
                    .any(|endpoint| endpoint.origin().contains("unverified"))
                {
                    return Err(Rejected::Unverified("node serves another network".into()));
                }
                Ok((Self::offline(), Vec::new()))
            })
        })
    }

    /// The same app, serving `statistics` as `network`'s one endpoint.
    pub(super) fn with_rpc(self, network: Network, statistics: json_rpc::Statistics) -> Self {
        let configured = LiveNetwork::find(&self.state.networks, &network).expect("a test network");
        let (nodes, _) = configured.current();
        configured.install(nodes, (Self::offline(), vec![statistics]));
        self
    }

    /// The same app, with `network` listed and not served, as when its
    /// node failed verification at startup.
    pub(super) fn unavailable(mut self, network: &Network) -> Self {
        let networks = Self::networks().map(|each| {
            let served = each != *network;
            Self::configured(each, served)
        });
        self.state.networks = networks.into();
        self.router = Self::route(self.state.clone());
        self
    }

    /// The same app with its supervisor running, so a rescan's pause is
    /// acknowledged.
    pub(super) fn supervised(mut self) -> Self {
        let supervisor = self.supervisor.take().expect("not yet running");
        tokio::spawn(supervisor.run(self.stop.subscribe()));
        self
    }

    /// What a running rescan of `network` holds: its wallets paused and
    /// its one change taken.
    pub(super) fn rescanning(
        &self,
        network: &Network,
    ) -> (wallets::Suspension, tokio::sync::OwnedMutexGuard<()>) {
        let configured = LiveNetwork::find(&self.state.networks, network).expect("a test network");
        let held = configured.maintain().expect("no change is running");
        (self.state.wallets.pause(network), held)
    }

    /// `network`'s stored node documents; empty before any is stored.
    pub(super) async fn stored(&self, network: &Network) -> Vec<Value> {
        let stored = RpcEndpoints::find_all(&self.state.pool)
            .await
            .expect("readable");
        stored
            .into_iter()
            .find(|(stored, _)| stored == network)
            .map(|(_, endpoints)| endpoints.documents())
            .unwrap_or_default()
    }

    /// Every wallet's id, archived ones included, in id order.
    pub(super) async fn ids(&self) -> Vec<String> {
        let rows = self.state.registry.wallets().await.expect("readable");
        rows.iter().map(|row| row.id().to_owned()).collect()
    }

    fn route(state: AppState) -> axum::Router {
        let token = http_server::BearerToken::new(TOKEN).expect("a bearer token");
        let config = http_server::Config::new(
            "127.0.0.1:0".parse().expect("a bind address"),
            http_server::TransportSecurity::PlaintextLoopback,
            Some(token),
            http_server::RequestLimits::default(),
        );
        router(state, &config).expect("the router builds")
    }

    pub(super) fn bitcoin() -> Network {
        Network::new(ChainName::new("bitcoin"), "regtest")
    }

    pub(super) fn solana() -> Network {
        Network::new(ChainName::new("solana"), "localnet")
    }

    pub(super) fn ethereum() -> Network {
        Network::new(ChainName::new("ethereum"), "mainnet")
    }

    /// `addr-destination` in `btc`'s network, as its factory parses it.
    pub(super) fn destination() -> base::Address {
        base::Address::new(Self::bitcoin(), "addr-destination")
    }

    fn networks() -> [Network; 3] {
        [Self::bitcoin(), Self::solana(), Self::ethereum()]
    }

    /// What each network's configuration would declare: its own coin.
    fn units() -> base::Units {
        Self::networks()
            .into_iter()
            .map(|network| {
                let asset = base::AssetId::native(network.chain.clone());
                ((network, asset), base::Unit::native(&COIN))
            })
            .collect()
    }

    /// Every network in `state` at checkpoint 8; a quarantined one also
    /// carries the failure the routes report.
    pub(super) fn health(state: NetworkState) -> SyncHealth {
        let networks = Self::networks();
        let mut health = SyncHealth::starting(&networks);
        for network in &networks {
            health.apply(&Self::reported(network, state));
        }
        health
    }

    /// One network's row in `state`, as the monitor publishes it.
    pub(super) fn reported(network: &Network, state: NetworkState) -> NetworkHealth {
        let quarantined = state == NetworkState::Quarantined;
        let mut row = NetworkHealth {
            state,
            failure: quarantined.then(|| {
                IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "block contains a duplicate transaction",
                    false,
                )
            }),
            attempts: u32::from(quarantined),
            since: 1_758_614_400,
            ..NetworkHealth::starting(network)
        };
        row.status.checkpoint = Some(indexing::testing::block(8));
        row
    }

    /// Publishes `health` as the supervisor's monitor would.
    pub(super) fn publish(&self, health: SyncHealth) {
        let ready = self.ready.as_ref().expect("health is still published");
        ready.send(health).expect("the routes hold the receiver");
    }

    /// Stops publishing health, as a monitor that has shut down does.
    pub(super) fn stop_publishing(&mut self) {
        self.ready = None;
    }

    /// Mines every network to block 9, paying every watched address, and
    /// indexes it.
    pub(super) async fn mine(&self) {
        let index = self.chains.index();
        for chain in self.chains.chains() {
            let network = chain.network();
            let produced = chain.tip().await.expect("a produced block").height.0;
            let watched = index.watched(network).await.expect("the watch list");
            let addresses: Vec<_> = watched.into_iter().map(|watch| watch.address).collect();
            let due = PAYOUTS.into_iter().filter(|&(height, _)| {
                produced < height && height <= MINED_TIP && !addresses.is_empty()
            });
            for (height, amount) in due {
                let block = indexing::testing::block(height);
                chain.include(&block, Self::payout(network, height, amount, &addresses));
            }
            chain.extend_to(MINED_TIP);
        }
        for unit in index.units() {
            unit.sync().await.expect("every network indexes its blocks");
        }
    }

    /// One faucet transaction paying `amount` to each of `addresses`.
    fn payout(
        network: &Network,
        height: u64,
        amount: u64,
        addresses: &[indexing::Address],
    ) -> indexing::Transaction {
        let asset = indexing::AssetId::native(network.chain.clone());
        let movements = addresses
            .iter()
            .enumerate()
            .map(|(index, to)| {
                indexing::ValueMovement::transfer(
                    indexing::MovementId(index.to_string()),
                    asset.clone(),
                    indexing::Decimal::from(amount),
                    indexing::Address::new(network.clone(), "faucet"),
                    to.clone(),
                )
            })
            .collect();
        indexing::Transaction {
            id: indexing::TransactionRef::new(network.clone(), format!("payout-{height}")),
            movements,
            fee: None,
            order: 0,
            status: indexing::Status::Included {
                block: indexing::testing::block(height),
            },
        }
    }

    /// Every send that reached any asset's outbox; empty when nothing was
    /// sent.
    pub(super) fn sent(&self) -> Vec<Submission> {
        self.outboxes.iter().flat_map(Outbox::submissions).collect()
    }

    /// The outbox of the asset tagged `btc`.
    pub(super) fn bitcoin_outbox(&self) -> &Outbox {
        &self.outboxes[0]
    }

    /// A generated wallet of `asset`, as `POST /v1/wallets` answered it.
    pub(super) async fn create(&self, asset: &str) -> Value {
        let body = serde_json::json!({ "asset": asset });
        let created = self.call("POST", "/v1/wallets", Some(body), true).await;
        assert_eq!(created.status, StatusCode::CREATED, "{asset} wallet");
        created.json()
    }

    /// The id of a generated `btc` wallet.
    pub(super) async fn wallet_id(&self) -> String {
        self.create("btc").await["id"]
            .as_str()
            .expect("a wallet id")
            .to_owned()
    }

    /// A JSON request, with the bearer token when `authorized`.
    pub(super) async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        authorized: bool,
    ) -> Reply {
        let body = body.map(|value| value.to_string());
        self.raw(method, path, body.as_deref(), authorized, &[])
            .await
    }

    /// A request with exact body text and extra headers; a body is sent
    /// as JSON unless `headers` name another content type.
    pub(super) async fn raw(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        authorized: bool,
        headers: &[(&str, &str)],
    ) -> Reply {
        let mut request = Request::builder().method(method).uri(path);
        if authorized {
            request = request.header("authorization", format!("Bearer {TOKEN}"));
        }
        let typed = headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type"));
        if body.is_some() && !typed {
            request = request.header("content-type", "application/json");
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = request
            .body(Body::from(body.unwrap_or_default().to_owned()))
            .expect("a request");
        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("a response");
        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let body = response.into_body().collect().await.expect("a body");
        Reply {
            status,
            content_type,
            body: body.to_bytes(),
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}
