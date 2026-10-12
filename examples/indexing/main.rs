//! Indexing through the container: three chains named once each, one SQLite
//! repository attached slot by slot, `Payments::start` verifying, composing,
//! driving and waiting for readiness; a watch per chain, the change stream,
//! and the repository's own balance read. No funds move. Needs all three
//! local nodes (examples/README.md):
//! `cargo run -p payment-examples --example indexing`

use std::sync::Arc;

use indexing::{
    Birthday,
    sqlite::{Database, Repository},
};
use json_rpc::Client as _;
use payments::{Payments, config::Document};
use serde_json::{Value, json};
use wallets::WalletRepository as _;

type AnyError = Box<dyn std::error::Error + Send + Sync>;

/// The chains this scenario runs and the node of each.
const CONFIG: &str = include_str!("config.toml");

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    // 1. Local testing only: a `solana-test-validator --reset` is a new
    //    cluster every run, so `config.toml` cannot state its genesis hash and
    //    the node is asked for it. A deployment writes the hash it expects.
    let sections: Value = Document::toml(CONFIG).parse()?;
    let url = sections["solana"]["rpc"]["endpoints"][0]
        .as_str()
        .ok_or("[solana] states rpc.endpoints")?;
    let genesis: String = json_rpc::Endpoint::new(url)
        .connect()?
        .request(json_rpc::Request::once("getGenesisHash", json!([])))
        .await?
        .map_err(json_rpc::CallError::from)?
        .deserialize()?;
    let config = CONFIG.replace("<genesis-hash-from-the-node>", &genesis);

    // 2. One repository serves every network; the container takes a view of
    //    it per slot, and the instance stays here with its full surface.
    tokio::task::spawn_blocking(|| std::fs::create_dir_all(".index-data")).await??;
    let file = std::path::absolute(".index-data/indexing.db")?;
    let database = Database::open(file).await?;
    sqlx::raw_sql(include_str!("../../schema/sdk.sql"))
        .execute(database.pool())
        .await?;
    // The wallet rows live in the same file: filed here, then watched.
    let filed = wallets::storage::sqlite::Repository::new(database.pool().clone());
    let repo = Arc::new(Repository::new(database));

    // 3. The chains this application links, one line each; then the store,
    //    then run: nodes verified, chains composed, index driven, ready.
    let payments = Payments::<String>::from_toml(&config)?
        .with::<chain_bitcoin::Config<String>>("bitcoin")
        .with::<chain_ethereum::Config<String>>("ethereum")
        .with::<chain_solana::Config<String>>("solana")
        .with_store(repo.clone())
        .with_output_store(repo.clone())
        .start()
        .await?;
    if let Some(skipped) = payments.skipped.first() {
        return Err(skipped.to_string().into());
    }
    let mut changes = payments
        .changes()
        .ok_or("the default index slot notifies")?;

    // 4. One wallet per chain, watched from the block after its checkpoint.
    let mut wallets = Vec::new();
    for (asset, id) in [
        ("btc", "btc-alice"),
        ("eth", "eth-alice"),
        ("sol", "sol-alice"),
    ] {
        let wallet = payments.wallets.create(&asset.to_owned()).await?;
        filed
            .register(wallets::Registration {
                id: id.to_owned(),
                address: wallet.address().clone(),
                label: None,
                key: None,
            })
            .await?;
        payments.watch(id, wallet.as_ref(), Birthday::Fresh).await?;
        wallets.push((id, wallet));
    }

    // 5. Know when the index has seen a watch, then ask the repository.
    if let Ok(Ok(change)) =
        tokio::time::timeout(std::time::Duration::from_secs(10), changes.recv()).await
    {
        println!("{} reached {}", change.watch, change.block.height.0);
    }
    for (id, wallet) in &wallets {
        // Pending blocks are sampled before the store read, so a commit
        // landing in between is counted once.
        let (_, pending) = payments
            .pending
            .iter()
            .find(|(network, _)| *network == wallet.address().network)
            .ok_or("every wallet's network is composed")?;
        let sample = pending.sampled()?;
        let balance = repo
            .balance(wallet.as_ref())
            .await?
            .completed(wallet.as_ref(), sample);
        println!(
            "{id} {}: {} (behind {})",
            wallet.address().value,
            balance.net,
            balance.behind
        );
    }

    // 6. Operations, then the one safe shutdown order.
    for network in payments.networks.list() {
        println!(
            "{}/{}: {:?}",
            network.network.chain, network.network.name, network.condition
        );
    }
    payments.shutdown().await?;
    Ok(())
}
