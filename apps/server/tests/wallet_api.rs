//! End-to-end API tests: the composed `payment-server` binary against owned
//! Bitcoin and Ethereum fixtures, on its single-file storage.

#[path = "acceptance.rs"]
mod acceptance;
#[path = "bitcoin_node.rs"]
mod bitcoin_node;
#[path = "ethereum_node.rs"]
mod ethereum_node;
#[path = "server_api.rs"]
mod server_api;

use std::{net::SocketAddr, process::Stdio, sync::Arc, time::Duration};

use bitcoin::{CompressedPublicKey, Network, PrivateKey, PublicKey};
use bitcoin_node::{BitcoinNode, FundingOutput};
use ethereum_node::{EthereumNode, Transaction};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::process::{Child, Command};

const TOKEN: &str = "wallet-api-system-test";
const USDC_CONTRACT: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";

struct RunningApi {
    root: String,
    child: Child,
    _config: TempDir,
    _storage: Option<Arc<TempDir>>,
}

impl RunningApi {
    async fn stop(mut self) {
        let pid = self.child.id().expect("API process must have an ID");
        let status = std::process::Command::new("kill")
            .arg("-INT")
            .arg(pid.to_string())
            .status()
            .expect("API interrupt must be sent");
        assert!(status.success(), "API interrupt must succeed");
        match tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await {
            Ok(result) => {
                assert!(result.expect("API process must be reaped").success());
            }
            Err(_) => {
                self.child.start_kill().expect("API process must stop");
                self.child.wait().await.expect("API process must be reaped");
                panic!("API did not stop after its shutdown signal");
            }
        }
    }
}

impl Drop for RunningApi {
    fn drop(&mut self) {
        // A failed assertion must not leave the composed API running in the
        // background and holding its owned database connections open.
        let _ = self.child.start_kill();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bitcoin_wallet_is_generated_and_indexed() -> Result<(), Box<dyn std::error::Error>> {
    let node = BitcoinNode::start().await;
    let api = start_api(json!({
        "bitcoin": {
            "network": "regtest",
            "rpc": rpc_config(&node.rpc_url, true),
            "confirmation_depth": 1,
            "cadence": {"poll_millis": 10, "batch_size": 10}
        }
    }))
    .await?;

    let wallet = create_wallet(&api.root, "btc").await?;
    assert_eq!(wallet["asset"], "btc");
    assert_eq!(
        wallet["network"],
        json!({"chain": "bitcoin", "name": "regtest"})
    );
    let address = wallet["address"]
        .as_str()
        .expect("wallet response must contain an address");
    assert_eq!(wallet_summary(&api.root, wallet_id(&wallet)).await?, wallet);
    let transaction_id = node.fund(vec![FundingOutput::new(address, 100_000)]);
    node.mine();

    wait_balance(&api.root, wallet_id(&wallet), "0.001").await;
    let history = wait_history(&api.root, wallet_id(&wallet), &transaction_id).await;
    assert_eq!(history["transactions"][0]["network"]["chain"], "bitcoin");
    let history_text = serde_json::to_string(&history)?;
    assert!(history_text.contains(address));
    assert!(history_text.contains("\"amount\":\"0.001\""));

    let submitted = send(
        &api.root,
        wallet_id(&wallet),
        &bitcoin_destination(9),
        "0.0004",
    )
    .await?;
    node.confirm();
    wait_history(&api.root, wallet_id(&wallet), &submitted).await;
    let remaining = wait_atomic_below(&api.root, wallet_id(&wallet), 8, 60_000).await;
    assert!(remaining > 0, "Bitcoin change must remain in the wallet");

    let batch = send_batch(
        &api.root,
        &[
            (wallet_id(&wallet), bitcoin_destination(8), "0.0001"),
            (wallet_id(&wallet), bitcoin_destination(7), "0.0001"),
        ],
    )
    .await?;
    assert_eq!(batch.len(), 1, "Bitcoin must group compatible transfers");
    node.confirm();
    wait_history(&api.root, wallet_id(&wallet), &batch[0]).await;
    let remaining = wait_atomic_below(&api.root, wallet_id(&wallet), 8, 40_000).await;
    assert!(remaining > 0, "Bitcoin batch change must remain");

    api.stop().await;
    node.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ethereum_wallet_is_generated_and_indexed() -> Result<(), Box<dyn std::error::Error>> {
    let node = EthereumNode::start().await;
    let api = start_api(json!({
        "ethereum": {
            "network": "mainnet",
            "chain_id": 1,
            "rpc": rpc_config(&node.rpc_url, false),
            "cadence": {"poll_millis": 10, "batch_size": 10}
        }
    }))
    .await?;

    let wallet = create_wallet(&api.root, "eth").await?;
    assert_eq!(wallet["asset"], "eth");
    assert_eq!(
        wallet["network"],
        json!({"chain": "ethereum", "name": "mainnet"})
    );
    let address = wallet["address"]
        .as_str()
        .expect("wallet response must contain an address")
        .to_owned();
    assert_eq!(wallet_summary(&api.root, wallet_id(&wallet)).await?, wallet);
    let transaction_id = format!("0x{}", "71".repeat(32));
    node.append(vec![Transaction::native(
        0x71,
        format!("0x{}", "55".repeat(20)),
        address.clone(),
        10_000_000_000_000_000_000,
    )]);
    // Past the fixture's finality lag: the balance is a fold over what the
    // index committed, so the funding block has to be below the line.
    node.append(Vec::new());
    node.append(Vec::new());

    wait_balance(&api.root, wallet_id(&wallet), "10").await;
    let history = wait_history(&api.root, wallet_id(&wallet), &transaction_id).await;
    assert_eq!(
        transaction(&history, &transaction_id)["network"]["chain"],
        "ethereum"
    );
    let history_text = serde_json::to_string(&history)?;
    assert!(history_text.contains(&address));
    assert!(history_text.contains("\"amount\":\"10\""));

    let destination = format!("0x{}", "22".repeat(20));
    node.expect_send(address, destination.clone(), 1_000_000_000_000_000_000);
    let submitted = send(&api.root, wallet_id(&wallet), &destination, "1").await?;
    node.confirm();
    wait_history(&api.root, wallet_id(&wallet), &submitted).await;
    wait_balance(&api.root, wallet_id(&wallet), "9").await;

    let first = format!("0x{}", "33".repeat(20));
    let second = format!("0x{}", "44".repeat(20));
    node.expect_send(
        wallet["address"]
            .as_str()
            .expect("wallet address")
            .to_owned(),
        first.clone(),
        1_000_000_000_000_000_000,
    );
    node.expect_send(
        wallet["address"]
            .as_str()
            .expect("wallet address")
            .to_owned(),
        second.clone(),
        2_000_000_000_000_000_000,
    );
    let batch = send_batch(
        &api.root,
        &[
            (wallet_id(&wallet), first, "1"),
            (wallet_id(&wallet), second, "2"),
        ],
    )
    .await?;
    assert_eq!(
        batch,
        node.submitted_ids(),
        "Ethereum IDs must keep request order"
    );
    assert_eq!(
        node.submitted_nonces(),
        vec![1, 2],
        "Ethereum batch must reserve consecutive nonces after the earlier send"
    );
    node.confirm();
    for transaction in &batch {
        wait_history(&api.root, wallet_id(&wallet), transaction).await;
    }
    wait_balance(&api.root, wallet_id(&wallet), "6").await;

    api.stop().await;
    node.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usdc_wallet_is_generated_sent_and_indexed() -> Result<(), Box<dyn std::error::Error>> {
    let node = EthereumNode::start().await;
    let api = start_api(json!({
        "ethereum": {
            "network": "mainnet",
            "chain_id": 1,
            "rpc": rpc_config(&node.rpc_url, false),
            "cadence": {"poll_millis": 10, "batch_size": 10},
            "tokens": [{"contract": USDC_CONTRACT, "asset": "usdc", "decimals": 6}]
        }
    }))
    .await?;

    let eth = create_wallet(&api.root, "eth").await?;
    let usdc = create_wallet(&api.root, "usdc").await?;
    assert_eq!(eth["asset"], "eth");
    assert_eq!(usdc["asset"], "usdc");
    assert_eq!(
        usdc["network"],
        json!({"chain": "ethereum", "name": "mainnet"})
    );
    assert_ne!(
        eth["address"], usdc["address"],
        "each generated asset wallet must have its own address"
    );

    let mixed = batch_response(
        &api.root,
        &[
            (wallet_id(&eth), format!("0x{}", "31".repeat(20)), "0.1"),
            (wallet_id(&usdc), format!("0x{}", "32".repeat(20)), "1"),
        ],
    )
    .await?;
    assert_eq!(mixed.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = mixed.json().await?;
    assert_eq!(body["failed_index"], 1);
    assert!(body["transaction_ids"].as_array().is_none_or(Vec::is_empty));
    assert!(
        node.submitted_ids().is_empty(),
        "mixed ETH and USDC assets must be rejected before broadcast"
    );

    let usdc_address = usdc["address"]
        .as_str()
        .expect("USDC wallet response must contain an address")
        .to_owned();
    let incoming = format!("0x{}", "72".repeat(32));
    node.append(vec![Transaction::erc20(
        0x72,
        USDC_CONTRACT.to_owned(),
        format!("0x{}", "55".repeat(20)),
        usdc_address.clone(),
        10_000_000,
    )]);
    // Past the fixture's finality lag, as the native test explains.
    node.append(Vec::new());
    node.append(Vec::new());

    wait_balance(&api.root, wallet_id(&usdc), "10").await;
    let incoming_history = wait_history(&api.root, wallet_id(&usdc), &incoming).await;
    let incoming_entry = transaction(&incoming_history, &incoming);
    assert_eq!(incoming_entry["network"]["chain"], "ethereum");
    assert_eq!(incoming_entry["movements"][0]["asset"]["id"], USDC_CONTRACT);
    assert_eq!(incoming_entry["movements"][0]["asset"]["decimals"], 6);
    assert_eq!(incoming_entry["movements"][0]["amount"], "10");
    assert_eq!(incoming_entry["movements"][0]["to"]["value"], usdc_address);

    let destination = format!("0x{}", "23".repeat(20));
    node.expect_token_send(
        USDC_CONTRACT.to_owned(),
        usdc_address,
        destination.clone(),
        1_000_000,
    );
    let submitted = send(&api.root, wallet_id(&usdc), &destination, "1").await?;
    node.confirm();
    let outgoing_history = wait_history(&api.root, wallet_id(&usdc), &submitted).await;
    let outgoing_entry = transaction(&outgoing_history, &submitted);
    assert_eq!(outgoing_entry["movements"][0]["asset"]["id"], USDC_CONTRACT);
    assert_eq!(outgoing_entry["movements"][0]["amount"], "1");
    assert_eq!(outgoing_entry["movements"][0]["to"]["value"], destination);
    assert_eq!(outgoing_entry["fee"]["asset"]["id"], "native");
    assert_eq!(outgoing_entry["fee"]["asset"]["decimals"], 18);
    wait_balance(&api.root, wallet_id(&usdc), "9").await;

    api.stop().await;
    node.stop().await;
    Ok(())
}

async fn start_api(chains: Value) -> Result<RunningApi, Box<dyn std::error::Error>> {
    start_api_with(chains, json!([]), &[]).await
}

async fn start_api_with(
    chains: Value,
    wallets: Value,
    secrets: &[(&str, &str)],
) -> Result<RunningApi, Box<dyn std::error::Error>> {
    start_api_in(chains, wallets, secrets, Arc::new(TempDir::new()?)).await
}

/// Starts the server on the index file inside `storage`. Passing the same
/// directory twice restarts the server on the index it already wrote.
async fn start_api_in(
    chains: Value,
    wallets: Value,
    secrets: &[(&str, &str)],
    storage: Arc<TempDir>,
) -> Result<RunningApi, Box<dyn std::error::Error>> {
    let configured = json!({"path": storage.path().join("payment.db")});
    let secrets = secrets
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect::<Vec<_>>();
    start_server(configured, chains, wallets, &secrets, Some(storage)).await
}

/// Starts the composed server binary on `storage`, with its secrets in the
/// environment, and waits until it is ready.
async fn start_server(
    storage: Value,
    chains: Value,
    wallets: Value,
    secrets: &[(String, String)],
    storage_directory: Option<Arc<TempDir>>,
) -> Result<RunningApi, Box<dyn std::error::Error>> {
    let bind = unused_address();
    let config_dir = TempDir::new()?;
    let config_path = config_dir.path().join("payment-api.json");
    std::fs::write(
        &config_path,
        serde_json::to_vec(&json!({
            "bind": bind,
            "bearer_token_env": "SYSTEM_TEST_TOKEN",
            "storage": storage,
            "chains": chains,
            "wallets": wallets
        }))?,
    )?;
    let child = Command::new(env!("CARGO_BIN_EXE_payment-server"))
        .arg(config_path)
        .env("SYSTEM_TEST_TOKEN", TOKEN)
        .env(
            "PAYMENT_WALLET_PASSPHRASE",
            "offline-registry-fixture-passphrase",
        )
        .envs(secrets.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    let running = RunningApi {
        root: format!("http://{bind}"),
        child,
        _config: config_dir,
        _storage: storage_directory,
    };
    wait_ready(&running.root).await;
    let unauthorized = reqwest::Client::new()
        .post(format!("{}/v1/wallets", running.root))
        .json(&json!({"asset": "btc"}))
        .send()
        .await?;
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
    Ok(running)
}

/// One node states its own headers and bounds: Bitcoin Core's credential is
/// one of its headers.
fn rpc_config(endpoint: &str, bitcoin: bool) -> Value {
    let headers = if bitcoin {
        json!({"authorization": "Basic test"})
    } else {
        json!({})
    };
    json!({
        "endpoints": [{
            "url": endpoint,
            "headers": headers,
            "bounds": {"timeout_ms": 2000, "max_response_bytes": 1048576}
        }]
    })
}

async fn create_wallet(root: &str, asset: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let response = reqwest::Client::new()
        .post(format!("{root}/v1/wallets"))
        .bearer_auth(TOKEN)
        .json(&json!({"asset": asset}))
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(format!("wallet creation failed with {status}: {body}").into());
    }
    Ok(serde_json::from_str(&body)?)
}

async fn wallet_summary(root: &str, wallet: &str) -> Result<Value, reqwest::Error> {
    reqwest::Client::new()
        .get(format!("{root}/v1/wallets/{wallet}"))
        .bearer_auth(TOKEN)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
}

async fn send(
    root: &str,
    wallet: &str,
    destination: &str,
    amount: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let response: Value = reqwest::Client::new()
        .post(format!("{root}/v1/wallets/{wallet}/transactions"))
        .bearer_auth(TOKEN)
        .json(&json!({
            "destination": destination,
            "amount": amount
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(response["transaction_id"]
        .as_str()
        .expect("submission must contain a transaction ID")
        .to_owned())
}

async fn send_batch(
    root: &str,
    transfers: &[(&str, String, &str)],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let response = batch_response(root, transfers).await?;
    let status = response.status();
    if !status.is_success() {
        let body: Value = response.json().await?;
        let message = body["message"]
            .as_str()
            .unwrap_or("missing public error message");
        return Err(std::io::Error::other(format!(
            "batch submission returned {status}: {message}"
        ))
        .into());
    }
    let response: Value = response.json().await?;
    Ok(response["transaction_ids"]
        .as_array()
        .expect("batch response must contain transaction IDs")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("transaction ID must be text")
                .to_owned()
        })
        .collect())
}

async fn batch_response(
    root: &str,
    transfers: &[(&str, String, &str)],
) -> Result<reqwest::Response, reqwest::Error> {
    let transfers = transfers
        .iter()
        .map(|(wallet, destination, amount)| {
            json!({
                "wallet_id": wallet,
                "destination": destination,
                "amount": amount
            })
        })
        .collect::<Vec<_>>();
    reqwest::Client::new()
        .post(format!("{root}/v1/transactions"))
        .bearer_auth(TOKEN)
        .json(&json!({"transfers": transfers}))
        .send()
        .await
}

async fn wait_balance(root: &str, wallet: &str, expected: &str) {
    let url = format!("{root}/v1/wallets/{wallet}/balance");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let reached = get(&url)
            .await
            .is_some_and(|value| value["amount"] == expected);
        if reached {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "wallet balance did not become {expected}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_atomic_below(root: &str, wallet: &str, decimals: u32, ceiling: u64) -> u64 {
    let url = format!("{root}/v1/wallets/{wallet}/balance");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last = None;
    loop {
        let amount = get(&url).await.and_then(|value| {
            value["amount"]
                .as_str()?
                .parse::<base::Decimal>()
                .ok()?
                .to_atomic_u64(decimals)
                .ok()
        });
        match amount {
            Some(amount) if amount < ceiling => return amount,
            Some(amount) => last = Some(amount),
            None => {}
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "wallet balance did not fall below {ceiling} atomic units; last balance: {last:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_history(root: &str, wallet: &str, transaction: &str) -> Value {
    let url = format!("{root}/v1/wallets/{wallet}/transactions?limit=20");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut last = None;
    loop {
        let response = get(&url).await;
        // Both halves are the wallet's history: what has settled, and what is
        // still in blocks above the confirmed line.
        let present = response
            .as_ref()
            .into_iter()
            .flat_map(|value| [&value["transactions"], &value["pending"]])
            .filter_map(|list| list.as_array())
            .flatten()
            .any(|item| item["transaction_id"] == transaction);
        match response {
            Some(value) if present => return value,
            Some(value) => last = Some(value),
            None => {}
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "wallet history did not contain transaction {transaction}; last response: {last:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One transaction of a history page, settled or still above the line.
fn transaction<'a>(history: &'a Value, transaction_id: &str) -> &'a Value {
    [&history["transactions"], &history["pending"]]
        .into_iter()
        .filter_map(|list| list.as_array())
        .flatten()
        .find(|transaction| transaction["transaction_id"] == transaction_id)
        .expect("requested transaction must be present in history")
}

async fn get(url: &str) -> Option<Value> {
    reqwest::Client::new()
        .get(url)
        .bearer_auth(TOKEN)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()
}

async fn wait_ready(root: &str) {
    let url = format!("{root}/healthz");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let ready = reqwest::get(&url)
            .await
            .is_ok_and(|response| response.status().is_success());
        if ready {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "wallet API did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn wallet_id(wallet: &Value) -> &str {
    wallet["id"]
        .as_str()
        .expect("wallet response must contain an ID")
}

fn unused_address() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("temporary listener must bind")
        .local_addr()
        .expect("temporary address must exist")
}

fn bitcoin_destination(byte: u8) -> String {
    let private = PrivateKey::from_slice(&[byte; 32], Network::Regtest)
        .expect("fixed destination key must be valid");
    let public = PublicKey::from_private_key(&bitcoin::secp256k1::Secp256k1::new(), &private);
    bitcoin::Address::p2wpkh(
        &CompressedPublicKey::try_from(public).expect("public key must be compressed"),
        Network::Regtest,
    )
    .to_string()
}
