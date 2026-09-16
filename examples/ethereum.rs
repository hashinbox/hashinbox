//! Uncomment a sample call in `main`, then run:
//! `cargo run --locked -p payment-examples --bin ethereum_wallet`.
//!
//! Sample 1 runs locally without a node or database. Samples 2 and 3 require
//! local Anvil: `anvil --host 127.0.0.1 --chain-id 31337 --hardfork cancun --timestamp 0`.
//! Connected samples open `.redb-data/ethereum.redb` without synchronizing.
//! Only the public address is printed; keys and wallet registrations remain in
//! memory and are dropped at exit.

use std::{sync::Arc, time::Duration};

use base::{Addresser, BlockPosition};
use chain_ethereum::{
    AccountClient, AssetKind, ETH, IndexerSettings, KeyPair, Limits, Network, RpcClient,
    TransactionClient, TransactionCoordinator, WalletConfig, WalletProvider, Wei,
};
use indexing::{Checkpoint, IndexScope};
use wallets::AddressFormat;

type AnyError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    // Sample 1: generate a local key and print its address.
    // generate_local_key()?;

    // Sample 2: attach a local key and use the operational wallet directly.
    // attach_local_wallet().await?;

    // Sample 3: insert an attached wallet into the chain-neutral collection.
    // insert_local_wallet().await?;

    Ok(())
}

#[allow(
    dead_code,
    reason = "Enable this sample by uncommenting its call in main."
)]
fn generate_local_key() -> Result<(), AnyError> {
    let key = KeyPair::generate()?;
    let address = key.address_text(&key.address())?;
    println!("Wallet address: {}", address.text);
    Ok(())
}

#[allow(
    dead_code,
    reason = "Enable this sample by uncommenting its call in main."
)]
async fn attach_local_wallet() -> Result<(), AnyError> {
    let (provider, _, _) = connect_provider().await?;
    let key = KeyPair::generate()?;
    let wallet: Arc<dyn wallets::Wallet> = provider.attach(key)?;
    let address = wallet.address_text(&wallet.address())?;
    println!("Wallet address: {}", address.text);
    Ok(())
}

#[allow(
    dead_code,
    reason = "Enable this sample by uncommenting its call in main."
)]
async fn insert_local_wallet() -> Result<(), AnyError> {
    let (provider, checkpoints, scope) = connect_provider().await?;
    let key = KeyPair::generate()?;
    let wallet: Arc<dyn wallets::Wallet> = provider.attach(key)?;
    let sender = provider.transactions();
    let mut wallets = wallets::Wallets::new(checkpoints);
    wallets.register("eth", scope, provider, sender, None)?;
    let info = wallets
        .insert("local-ethereum", &"eth", wallet, BlockPosition(0))
        .await?;
    println!("Wallet address: {}", info.address.text);
    Ok(())
}

async fn connect_provider() -> Result<(WalletProvider, Arc<dyn Checkpoint>, IndexScope), AnyError> {
    // Configure transport and verify the node before opening storage.
    let rpc_config = json_rpc::Config::new("http://127.0.0.1:8545", Duration::from_secs(15));
    let rpc = RpcClient::new(json_rpc::Http::new(rpc_config)?);

    let settings = IndexerSettings::new(Network::new(31_337, "anvil"));
    let scope = settings.scope();
    let source = settings.source(rpc.clone()).await?;

    // Indexing policy remains independent of the RPC transport.
    let db_dir = std::path::absolute(".redb-data")?;
    let storage = tokio::task::spawn_blocking(move || -> Result<_, AnyError> {
        std::fs::create_dir_all(&db_dir)?;
        Ok(storage_redb::Redb::open(db_dir.join("ethereum.redb"))?)
    })
    .await??;
    let repository = indexing_redb::Repository::new(storage, scope.clone())?;
    let indexer = Arc::new(settings.build(source, repository, None)?);

    // Compose shared account, transaction, and history capabilities.
    let limits = Limits::new(
        128 * 1024,                                 // Maximum transaction input bytes.
        2_000,                                      // 20% gas-limit margin.
        30_000_000,                                 // Maximum gas limit.
        Wei::from_u128(1_000_000_000_000),          // 1,000 gwei max fee per gas.
        Wei::from_u128(100_000_000_000),            // 100 gwei max priority fee per gas.
        Wei::from_u128(10_000_000_000_000_000_000), // 10 ETH max total fee.
    )?;
    let accounts = Arc::new(AccountClient::new(rpc.clone(), settings.network.chain_id)?);
    let transactions = Arc::new(TransactionClient::new(
        rpc,
        settings.network.chain_id,
        limits,
    )?);
    let coordinator = Arc::new(TransactionCoordinator::new(accounts.clone(), transactions));
    let provider = WalletProvider::new(
        WalletConfig {
            scope: scope.clone(),
            chain_id: settings.network.chain_id,
            asset: AssetKind::Native,
            decimals: ETH.decimals,
        },
        accounts,
        coordinator,
        indexer.clone(),
    );
    Ok((provider, indexer, scope))
}
