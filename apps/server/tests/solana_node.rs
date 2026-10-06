//! Explicit manual system target for the owned Agave stack on the index file.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    future::Future,
    io,
    net::TcpListener,
    path::{Path, PathBuf},
    pin::Pin,
    process::{Child, Command, Stdio},
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

use base::{BlockPosition, Decimal, Networked};
use chain_solana::{IndexerParams, Network, RpcClient, WalletConfig, WalletFactory};
use indexing::{
    Birthday, Checkpoint as _, IndexStore as _, Indexing as _, OutputRequest, OutputStore,
    WatchStore as _, Watching as _,
};
use sha2::{Digest, Sha256};
use tokio::{task::JoinSet, time::sleep};
use wallets::{SecretBytes, Wallets};

const RELEASE: &str = include_str!("fixtures/agave-v3.1.14.sha256");
const VERSION: &str = "v3.1.14";
const COMMIT: &str = "3134055b562e95902233be308453fffa1c4a8902";
/// The key this test registers native SOL under. Any ordered key serves
/// `wallets::Wallets`; the server's own tag type lives inside the binary.
const SOL: &str = "sol";

#[derive(Debug, PartialEq, Eq)]
struct Artifact<'a> {
    target: &'a str,
    name: &'a str,
    sha256: &'a str,
}

fn artifacts() -> Result<Vec<Artifact<'static>>, io::Error> {
    let mut lines = RELEASE.lines();
    if lines.next() != Some(&format!("version {VERSION}"))
        || lines.next() != Some(&format!("commit {COMMIT}"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Agave release identity is invalid",
        ));
    }
    lines
        .map(|line| {
            let mut fields = line.split_ascii_whitespace();
            let artifact = Artifact {
                target: fields.next().ok_or_else(invalid_manifest)?,
                name: fields.next().ok_or_else(invalid_manifest)?,
                sha256: fields.next().ok_or_else(invalid_manifest)?,
            };
            let checksum_is_canonical = artifact.sha256.len() == 64
                && artifact
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
            if fields.next().is_some() || !checksum_is_canonical {
                return Err(invalid_manifest());
            }
            Ok(artifact)
        })
        .collect()
}

fn invalid_manifest() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid Agave checksum manifest",
    )
}

fn artifact_for<'a>(
    artifacts: &'a [Artifact<'a>],
    target: &str,
) -> Result<&'a Artifact<'a>, io::Error> {
    artifacts
        .iter()
        .find(|artifact| artifact.target == target)
        .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "unsupported Agave platform"))
}

fn verify(path: &Path, expected: &str) -> Result<(), io::Error> {
    let actual = format!("{:x}", Sha256::digest(fs::read(path)?));
    if actual == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Agave artifact checksum mismatch",
        ))
    }
}

fn archive(artifact: &Artifact<'_>) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/solana-stack")
        .join(VERSION)
        .join(artifact.name)
}

fn extract(archive: &Path, destination: &Path) -> Result<PathBuf, io::Error> {
    let output = Command::new("tar")
        .args(["-xjf"])
        .arg(archive)
        .args(["-C"])
        .arg(destination)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            "could not extract reviewed Agave artifact",
        ));
    }
    executable(destination, "solana-test-validator").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "reviewed artifact does not contain solana-test-validator",
        )
    })
}

fn executable(directory: &Path, name: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(directory).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            let Some(found) = executable(&path, name) else {
                continue;
            };
            return Some(found);
        }
        let matches_name = path.file_name().and_then(|value| value.to_str()) == Some(name);
        if matches_name {
            return Some(path);
        }
    }
    None
}

fn host_target() -> &'static str {
    if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
        "x86_64-unknown-linux-gnu"
    } else {
        "unsupported"
    }
}

#[test]
fn executable_search_finds_nested_files_and_rejects_missing_or_file_roots() {
    let directory = tempfile::tempdir().expect("temporary search directory");
    let named_directory = directory.path().join("fixture-tool");
    fs::create_dir(&named_directory).expect("same-named directory");
    assert_eq!(executable(directory.path(), "fixture-tool"), None);

    let nested = directory.path().join("release/bin");
    fs::create_dir_all(&nested).expect("nested artifact layout");
    let expected = nested.join("fixture-tool");
    fs::write(&expected, b"fixture only").expect("fixture file");
    assert_eq!(
        executable(directory.path(), "fixture-tool"),
        Some(expected.clone())
    );
    assert_eq!(executable(&expected, "fixture-tool"), None);
    assert_eq!(
        executable(&directory.path().join("missing"), "fixture-tool"),
        None
    );
}

#[test]
fn pins_the_reviewed_release_and_platform_artifacts() {
    let artifacts = artifacts().expect("reviewed checksum manifest");
    assert_eq!(artifacts.len(), 3);
    assert!(artifact_for(&artifacts, host_target()).is_ok());
}

#[test]
fn rejects_unsupported_platform_and_corrupt_artifact() {
    let artifacts = artifacts().expect("reviewed checksum manifest");
    assert_eq!(
        artifact_for(&artifacts, "mips64-unknown-none")
            .expect_err("unsupported platform")
            .kind(),
        io::ErrorKind::Unsupported
    );

    let directory = tempfile::tempdir().expect("temporary artifact directory");
    let path = directory.path().join("corrupt.tar.bz2");
    fs::write(&path, b"not the reviewed artifact").expect("corrupt fixture");
    assert_eq!(
        verify(&path, artifacts[0].sha256)
            .expect_err("checksum mismatch")
            .kind(),
        io::ErrorKind::InvalidData
    );
}

struct Ports {
    _reservations: Vec<TcpListener>,
    rpc: u16,
    faucet: u16,
    gossip: u16,
    dynamic: (u16, u16),
}

impl Ports {
    fn reserve() -> Result<Self, io::Error> {
        let seed = 20_000 + (std::process::id() % 1_000) as u16 * 32;
        for offset in 0..1_000_u16 {
            let Some(base) = seed.checked_add(offset * 32) else {
                break;
            };
            let Some(end) = base.checked_add(30) else {
                break;
            };
            // Agave 3.1.14 requires max - min >= 25 for dynamic ports.
            let candidates = (base..end).collect::<Vec<_>>();
            let reservations = candidates
                .iter()
                .map(|port| TcpListener::bind(("127.0.0.1", *port)))
                .collect::<Result<Vec<_>, _>>();
            if let Ok(reservations) = reservations {
                return Ok(Self {
                    _reservations: reservations,
                    rpc: base,
                    faucet: base + 2,
                    gossip: base + 3,
                    dynamic: (base + 4, base + 29),
                });
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "could not reserve isolated validator ports",
        ))
    }
}

struct Validator {
    child: Child,
    directory: tempfile::TempDir,
    endpoint: String,
}

impl Validator {
    fn start(binary: &Path) -> Result<Self, io::Error> {
        let directory = tempfile::tempdir()?;
        let ledger = directory.path().join("ledger");
        let log = File::create(directory.path().join("validator.log"))?;
        let ports = Ports::reserve()?;
        let endpoint = format!("http://127.0.0.1:{}", ports.rpc);
        let dynamic = format!("{}-{}", ports.dynamic.0, ports.dynamic.1);
        let mut command = Command::new(binary);
        command
            .args(["--ledger"])
            .arg(&ledger)
            .args(["--bind-address", "127.0.0.1", "--rpc-port"])
            .arg(ports.rpc.to_string())
            .args(["--faucet-port"])
            .arg(ports.faucet.to_string())
            .args(["--gossip-port"])
            .arg(ports.gossip.to_string())
            .args(["--dynamic-port-range", &dynamic, "--reset", "--log"])
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        drop(ports);
        Ok(Self {
            child: command.spawn()?,
            directory,
            endpoint,
        })
    }

    fn log(&self) -> String {
        let path = self.directory.path().join("validator.log");
        match fs::read_to_string(&path) {
            Ok(log) => log
                .lines()
                .rev()
                .take(80)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n"),
            Err(error) => format!("could not read {}: {error}", path.display()),
        }
    }
}

impl Drop for Validator {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
struct Tasks(Mutex<JoinSet<()>>);

impl wallets::SubmissionRegistrar for Tasks {
    fn register<'a>(
        &'a self,
        task: wallets::SubmissionTask,
    ) -> Pin<Box<dyn Future<Output = Result<(), wallets::RegistrationError>> + Send + 'a>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .spawn(task.run());
            Ok(())
        })
    }
}

impl Tasks {
    async fn drain(&self) {
        let mut tasks = {
            let mut owned = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *owned)
        };
        while let Some(result) = tasks.join_next().await {
            result.expect("owned submission task");
        }
    }
}

/// One raw JSON-RPC call, outside the SDK, for fixture setup.
async fn call(endpoint: &str, method: &str, params: serde_json::Value) -> serde_json::Value {
    let response = reqwest::Client::new()
        .post(endpoint)
        .json(&serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
        .send()
        .await
        .expect("owned validator response");
    assert!(response.status().is_success());
    let body: serde_json::Value = response.json().await.expect("JSON-RPC body");
    body["result"].clone()
}

/// Waits for the owned validator, then connects to it as the cluster whose
/// genesis hash it reports.
async fn ready(validator: &mut Validator) -> (Network, RpcClient<json_rpc::Http>) {
    let endpoint = &validator.endpoint;
    // One node, no decorator: the chain's own client already states
    // `Request::once` for every call.
    let node = json_rpc::Endpoint::new(endpoint).with_bounds(json_rpc::Bounds {
        timeout_ms: 2_000,
        max_request_bytes: 1_048_576,
        ..json_rpc::Bounds::default()
    });
    for _ in 0..300 {
        if let Some(status) = validator.child.try_wait().expect("validator status") {
            panic!(
                "owned validator exited with {status}; log:\n{}",
                validator.log()
            );
        }
        let healthy = reqwest::Client::new()
            .post(endpoint)
            .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getHealth","params":[]}))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success());
        if healthy && call(endpoint, "getHealth", serde_json::json!([])).await == "ok" {
            let genesis = call(endpoint, "getGenesisHash", serde_json::json!([])).await;
            let genesis = genesis
                .as_str()
                .expect("genesis text")
                .parse()
                .expect("genesis");
            let network = Network::new("solana-stack", genesis);
            let transport = node
                .connect(&std::collections::BTreeMap::new())
                .expect("owned transport");
            let rpc = RpcClient::connect(transport, &network)
                .await
                .expect("exact genesis and bundled executable Memo-v3");
            return (network, rpc);
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "owned validator did not become ready; log:\n{}",
        validator.log()
    )
}

async fn airdrop(endpoint: &str, address: &str, lamports: u64) {
    let response = reqwest::Client::new()
        .post(endpoint)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "requestAirdrop",
            "params": [address, lamports, {"commitment": "finalized"}]
        }))
        .send()
        .await
        .expect("owned validator airdrop response");
    assert!(response.status().is_success());
    let body: serde_json::Value = response.json().await.expect("airdrop JSON");
    let signature = body["result"].as_str().expect("airdrop signature");
    finalized(endpoint, signature).await;
}

async fn finalized(endpoint: &str, signature: &str) {
    for _ in 0..300 {
        let statuses = call(
            endpoint,
            "getSignatureStatuses",
            serde_json::json!([[signature], {"searchTransactionHistory": true}]),
        )
        .await;
        let status = &statuses["value"][0];
        if !status.is_null() {
            assert!(
                status["err"].is_null(),
                "transaction {signature} failed: {status}"
            );
            if status["confirmationStatus"] == "finalized" {
                return;
            }
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("transaction {signature} did not finalize")
}

#[tokio::test(flavor = "multi_thread")]
async fn native_sol_submission_indexing_and_central_storage() {
    let artifacts = artifacts().expect("reviewed checksum manifest");
    let artifact = artifact_for(&artifacts, host_target()).expect("supported platform");
    let archive = archive(artifact);
    verify(&archive, artifact.sha256).expect("exact reviewed validator artifact");
    let extracted = tempfile::tempdir().expect("isolated extraction");
    let binary = extract(&archive, extracted.path()).expect("reviewed validator binary");
    let version = Command::new(&binary)
        .arg("--version")
        .output()
        .expect("validator version");
    assert!(String::from_utf8_lossy(&version.stdout).contains("3.1.14"));

    let mut validator = Validator::start(&binary).expect("owned validator");
    let (native, client) = ready(&mut validator).await;
    let network = native.canonical();

    let index = tempfile::tempdir().expect("isolated index directory");
    let file = indexing::sqlite::Database::open(index.path().join("solana.db"))
        .await
        .expect("index file");
    let repository = indexing::sqlite::Repository::new(file.clone());
    let service = Arc::new(
        chain_solana::indexer(IndexerParams {
            ..IndexerParams::new(client.clone(), repository.clone())
        })
        .expect("Solana indexer"),
    );
    let tasks = Arc::new(Tasks::default());
    // The sender's absence proof: the repository's own confirmed pages joined to
    // the blocks this index holds above its line, by the pair that belongs
    // together.
    let history = Arc::new(indexing::PendingJoin::new(
        repository.clone(),
        service.clone(),
    ));
    let factory = WalletFactory::new(
        WalletConfig::new(&native),
        &client,
        &service,
        history.clone(),
    )
    .expect("native SOL factory")
    .with_registrar(tasks.clone());
    let mut wallets = Wallets::<&str>::new();
    wallets.add(SOL, factory).expect("native SOL registration");
    let asset = wallets.asset(&SOL).expect("registered SOL asset").id;
    // What an amount of it is, from the chain that declares it — never from a
    // wallet.
    let unit = base::Unit::native(&chain_solana::SOL);
    let mut held = BTreeMap::new();
    for (id, seed) in [("source", 7_u8), ("destination", 9)] {
        let wallet = wallets
            .from(&SOL, SecretBytes::new(vec![seed; 32]))
            .await
            .expect("configured wallet");
        // This in-memory fixture keeps signing handles only after its watch
        // succeeds; production durable custody is owned by the server registry.
        service
            .watch(id, wallet.as_ref(), Birthday::At(BlockPosition(0)))
            .await
            .expect("watch");
        held.insert(id.to_owned(), wallet);
    }
    let source = held["source"].address().clone();
    let destination = held["destination"].address().clone();

    airdrop(&validator.endpoint, &source.value, 2_000_000).await;
    let rent = call(
        &validator.endpoint,
        "getMinimumBalanceForRentExemption",
        serde_json::json!([0]),
    )
    .await
    .as_u64()
    .expect("System account rent minimum");
    airdrop(&validator.endpoint, &destination.value, rent).await;
    let amount = "0.00001".parse::<Decimal>().expect("exact SOL amount");
    let transaction = wallets
        .author(&SOL, &held["source"])
        .expect("SOL is an asset this deployment sends")
        .send(destination.clone(), amount.clone().into())
        .await
        .expect("native SOL submission");
    tasks.drain().await;
    finalized(&validator.endpoint, &transaction.to_string()).await;

    let mut observed = None;
    for _ in 0..100 {
        service.sync().await.expect("sparse synchronization");
        observed = transactions(&*history, &asset, &source)
            .await
            .into_iter()
            .find(|entry| entry.transaction.id.id == transaction);
        if observed.is_some() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    if observed.is_none() {
        let status = call(
            &validator.endpoint,
            "getSignatureStatuses",
            serde_json::json!([[transaction], {"searchTransactionHistory": true}]),
        )
        .await;
        let checkpoint = service.checkpoint(&network).await.expect("checkpoint");
        panic!("submitted transaction not canonical: status={status}, checkpoint={checkpoint:?}");
    }
    let observed = observed.expect("submitted transaction became canonical");
    assert_eq!(observed.transaction.movements.len(), 1);
    assert_eq!(
        unit.display(&observed.transaction.movements[0].amount)
            .expect("exact SOL movement"),
        amount
    );
    assert_eq!(
        unit.display(&observed.transaction.fee.expect("exact network fee").amount)
            .expect("exact SOL fee"),
        "0.000005".parse::<Decimal>().expect("exact SOL fee")
    );

    let address = chain_solana::Address::from_str(&source.value).expect("source address");
    let outputs = repository
        .list(OutputRequest {
            network: network.clone(),
            address: address.canonical(&network),
            after: None,
            limit: 100,
        })
        .await
        .expect("Solana output projection");
    assert!(outputs.outputs.is_empty());

    // Solana's source reads only rooted blocks, so its tip is already its
    // confirmed line: everything this pass committed is irreversible, and a
    // restart must find exactly it rather than rebuild it.
    let checkpoint_before = service
        .checkpoint(&network)
        .await
        .expect("checkpoint before restart")
        .expect("persisted Solana checkpoint");

    drop(held);
    drop(service);
    let registered_before = repository
        .all(&network)
        .await
        .expect("observations before restart");
    assert!(!registered_before.is_empty());
    let restarted_repository = indexing::sqlite::Repository::new(file);
    let restarted_checkpoint = restarted_repository
        .checkpoint(&network)
        .await
        .expect("restarted checkpoint read");
    assert_eq!(
        restarted_checkpoint.as_ref(),
        Some(&checkpoint_before),
        "a restart finds the committed checkpoint unchanged"
    );
    let restarted_service = Arc::new(
        chain_solana::indexer(IndexerParams {
            ..IndexerParams::new(client.clone(), restarted_repository.clone())
        })
        .expect("restarted Solana indexer"),
    );
    let restored_history =
        indexing::PendingJoin::new(restarted_repository.clone(), restarted_service.clone());

    // Nothing re-watches anything: the restarted index reads its own durable
    // rows and resumes with the addresses it had.
    assert_eq!(
        restarted_service
            .watched(&network)
            .await
            .expect("restored watch list")
            .into_iter()
            .map(|watch| watch.id)
            .collect::<Vec<_>>(),
        ["destination", "source"],
        "a restart restores the watch list from the registry, in id order"
    );
    let mut restored = false;
    for _ in 0..100 {
        restarted_service
            .sync()
            .await
            .expect("restart synchronization");
        restored = transactions(&restored_history, &asset, &source)
            .await
            .iter()
            .any(|entry| entry.transaction.id.id == transaction);
        if restored {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(restored, "a restart serves the committed history it stored");
    assert!(
        restarted_repository
            .list(OutputRequest {
                network: network.clone(),
                address: address.canonical(&network),
                after: None,
                limit: 100,
            })
            .await
            .expect("restarted Solana output projection")
            .outputs
            .is_empty()
    );
    assert_eq!(
        restarted_repository
            .all(&network)
            .await
            .expect("observations after restart"),
        registered_before,
        "a restart preserves every observation and the progress recorded for it"
    );
}

/// One page of `address`'s confirmed history, exactly as the reader over this
/// index holds it: every amount atomic, which is the only unit `indexing` can
/// produce.
async fn transactions(
    history: &dyn indexing::History,
    asset: &base::AssetId,
    address: &base::Address,
) -> Vec<indexing::ObservedTransaction> {
    let subject = indexing::Subject {
        address: address.clone(),
        asset: asset.clone(),
    };
    history
        .history(&subject, indexing::Page::first(100))
        .await
        .expect("canonical SOL history")
        .transactions
}
