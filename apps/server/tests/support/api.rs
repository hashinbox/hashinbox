//! [`Api`]: the routes a case calls, through the router itself (no socket).

use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use base::NetworkConfig;
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

/// A generated, keyed wallet: its id and address.
pub(crate) struct Wallet {
    pub(crate) id: String,
    pub(crate) address: String,
}

/// The API of one [`super::App`].
#[derive(Clone)]
pub(crate) struct Api(Router);

impl Api {
    pub(super) const fn new(router: Router) -> Self {
        Self(router)
    }

    /// `method uri` with `body`: its status and JSON body.
    pub(crate) async fn call(
        &self,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
            .expect("a request");
        let response = self.0.clone().oneshot(request).await.expect("a response");
        let status = response.status();
        let bytes = response.into_body().collect().await.expect("a body");
        let body = serde_json::from_slice(&bytes.to_bytes()).unwrap_or(Value::Null);
        (status, body)
    }

    /// `GET uri`'s body.
    ///
    /// # Panics
    ///
    /// On any status but 200.
    pub(crate) async fn get(&self, uri: &str) -> Value {
        let (status, body) = self.call(Method::GET, uri, None).await;
        assert_eq!(status, StatusCode::OK, "GET {uri}: {body}");
        body
    }

    /// `POST uri` with `body`, expecting `status`.
    async fn post(&self, uri: &str, body: Value, expect: StatusCode) -> Value {
        let (status, answer) = self.call(Method::POST, uri, Some(body)).await;
        assert_eq!(status, expect, "POST {uri}: {answer}");
        answer
    }

    /// A new keyed wallet of `network`.
    pub(crate) async fn wallet(&self, network: &NetworkConfig) -> Wallet {
        let body =
            json!({"chain": network.network.chain.as_str(), "network": network.network.name});
        let created = self.post("/v1/wallets", body, StatusCode::CREATED).await;
        Wallet {
            id: created["id"].as_str().unwrap_or_default().to_owned(),
            address: created["address"].as_str().unwrap_or_default().to_owned(),
        }
    }

    /// Indexes `wallet` in `asset` from `start` (the tip when `None`).
    pub(crate) async fn index(&self, wallet: &Wallet, asset: &str, start: Option<u64>) -> Value {
        let mut body = json!({"asset": asset});
        if let Some(start) = start {
            body["start"] = json!(start);
        }
        let uri = format!("/v1/wallets/{}/indexes", wallet.id);
        self.post(&uri, body, StatusCode::CREATED).await
    }

    /// The index of `wallet` in `asset`, as the wallet read shows it.
    pub(crate) async fn index_of(&self, wallet: &Wallet, asset: &str) -> Value {
        let read = self.get(&format!("/v1/wallets/{}", wallet.id)).await;
        read["indexes"][asset].clone()
    }

    /// The mined legs of `wallet` in `asset`, latest first, its adjustments
    /// left out (the one opening an index on a chain that reads balances).
    pub(crate) async fn legs(&self, wallet: &Wallet, asset: &str) -> Vec<Value> {
        let uri = format!("/v1/wallets/{}/indexes/{asset}/legs", wallet.id);
        let page = self.get(&uri).await;
        let legs = page["legs"].as_array().cloned().unwrap_or_default();
        legs.into_iter().filter(|leg| leg["kind"] != 6).collect()
    }

    /// Sends `amount` (display units) of `asset` from `wallet` to `to`:
    /// the job's id.
    pub(crate) async fn send(
        &self,
        wallet: &Wallet,
        asset: &str,
        to: &str,
        amount: &str,
    ) -> String {
        let body = json!({"asset": asset, "transfers": [{"destination": to, "amount": amount}]});
        let uri = format!("/v1/wallets/{}/transactions", wallet.id);
        let accepted = self.post(&uri, body, StatusCode::ACCEPTED).await;
        accepted["job"].as_str().unwrap_or_default().to_owned()
    }

    /// Job `id`.
    pub(crate) async fn job(&self, id: &str) -> Value {
        self.get(&format!("/v1/jobs/{id}")).await
    }

    /// The live kept blocks of `network`, latest first (at most 500).
    pub(crate) async fn blocks(&self, network: &NetworkConfig) -> Vec<Value> {
        let (chain, name) = (network.network.chain.as_str(), &network.network.name);
        let page = self
            .get(&format!("/v1/networks/{chain}/{name}/blocks?limit=500"))
            .await;
        page["blocks"].as_array().cloned().unwrap_or_default()
    }

    /// The one network, as `GET /v1/networks` lists it.
    pub(crate) async fn network(&self) -> Value {
        self.get("/v1/networks").await["networks"][0].clone()
    }

    /// The network named `name`, as `GET /v1/networks` lists it.
    pub(crate) async fn network_named(&self, name: &str) -> Value {
        let networks = self.get("/v1/networks").await["networks"].clone();
        let networks = networks.as_array().cloned().unwrap_or_default();
        networks
            .into_iter()
            .find(|network| network["network"] == name)
            .unwrap_or(Value::Null)
    }
}
