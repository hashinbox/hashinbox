//! Uncomment a sample call in `main`, then run:
//! `cargo run --locked -p payment-examples --bin bitcoin_wallet`.
//!
//! Sample 1 runs locally without a node or database. Samples 2 and 3 require
//! Bitcoin Core 31.x on local regtest, synchronized and unpruned, with synchronized
//! `txindex`, RPC port 18443, and bitcoin/bitcoin RPC credentials.
//! Connected samples open `.redb-data/bitcoin.redb` without synchronizing.
//! Only the public address is printed; keys and wallet registrations remain in
//! memory and are dropped at exit.

use std::{sync::Arc, time::Duration};

use base::{Addresser, BlockPosition};
use chain_bitcoin::{
    AddressType, FeeRate, IndexUtxos, IndexerSettings, KeyPair, Network, RpcClient, WalletConfig,
    WalletProvider,
};
use indexing::{Checkpoint, IndexScope};
use wallets::{AddressFormat, Provider};

type AnyError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    // Sample 1: generate a local key and print its address.
    generate_local_address()?;

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
fn generate_local_address() -> Result<(), AnyError> {
    let key = KeyPair::generate(Network::Regtest, AddressType::SegwitV0)?;
    let address = key.address_text(&key.address())?;
    println!("Wallet address: {}", address.text);
    Ok(())
}

#[allow(
    dead_code,
    reason = "Enable this sample by uncommenting its call in main."
)]
async fn attach_local_wallet() -> Result<(), AnyError> {
    let network = Network::Regtest;
    let address_type = AddressType::SegwitV0;
    let (provider, _, _) = connect_provider(network, address_type).await?;
    let key = KeyPair::generate(network, address_type)?;
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
    let network = Network::Regtest;
    let address_type = AddressType::SegwitV0;
    let (provider, checkpoints, scope) = connect_provider(network, address_type).await?;
    let key = KeyPair::generate(network, address_type)?;
    let wallet: Arc<dyn wallets::Wallet> = provider.attach(key)?;
    let sender = provider.transactions();
    let mut wallets = wallets::Wallets::new(checkpoints);
    wallets.register("btc", scope, provider, sender, None)?;
    let info = wallets
        .insert("local-bitcoin", &"btc", wallet, BlockPosition(0))
        .await?;
    println!("Wallet address: {}", info.address.text);
    Ok(())
}

async fn connect_provider(
    network: Network,
    address_type: AddressType,
) -> Result<(WalletProvider, Arc<dyn Checkpoint>, IndexScope), AnyError> {
    // Configure transport and verify the node before opening storage.
    let rpc_config = json_rpc::Config::new(
        "http://bitcoin:bitcoin@127.0.0.1:18443",
        Duration::from_secs(15),
    );
    let rpc = RpcClient::connect(json_rpc::Http::new(rpc_config)?, network).await?;

    // Indexing policy remains independent of the RPC transport.
    let settings = IndexerSettings::new(network);
    let scope = settings.scope();
    let db_dir = std::path::absolute(".redb-data")?;
    let storage = tokio::task::spawn_blocking(move || -> Result<_, AnyError> {
        std::fs::create_dir_all(&db_dir)?;
        Ok(storage_redb::Redb::open(db_dir.join("bitcoin.redb"))?)
    })
    .await??;
    
    let repository = Arc::new(
        indexing_redb::Repository::new(storage, scope.clone()
    )?);

    let indexer = Arc::new(
        settings.build(
            rpc.clone(), 
            repository.as_ref().clone(), 
            None
        )
    ?);

    let config = WalletConfig {
        scope: scope.clone(),
        network,
        address_type,
        fee_target_blocks: 6,
        max_fee_rate: FeeRate::new(10_000),
    };

    let utxos = Arc::new(
        IndexUtxos::new(scope.clone(), network, repository)?
    );
    
    let provider = WalletProvider::new(
        config,
        utxos,
        Arc::new(rpc.fees()),
        Arc::new(rpc.transactions()),
        indexer.clone(),
    );

    // provider.

    Ok((provider, indexer, scope))
}
