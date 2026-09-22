//! Run with a local Anvil node:
//! `anvil --host 127.0.0.1 --chain-id 31337 --hardfork cancun --timestamp 0`.
//!
//! Then run:
//! `cargo run --locked -p payment-examples --bin ethereum_wallet`.
//! The example opens `.redb-data/ethereum.redb` and keeps keys in memory only.

use std::{sync::Arc, time::Duration};

use base::BlockPosition;
use chain_ethereum::{
    AccountClient, AssetKind, ETH, IndexerSettings, KeyPair, Limits, Network, RpcClient,
    TransactionClient, TransactionCoordinator, WalletConfig, WalletProvider, Wei,
};
use indexing::Checkpoint;

type AnyError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    let rpc_config = json_rpc::Config::new("http://127.0.0.1:8545", Duration::from_secs(15));
    let rpc = RpcClient::new(json_rpc::Http::new(rpc_config)?);

    let settings = IndexerSettings::new(Network::new(31_337, "anvil"));
    let scope = settings.scope();
    let source = settings.source(rpc.clone()).await?;

    let db_dir = std::path::absolute(".redb-data")?;
    let storage = tokio::task::spawn_blocking(move || -> Result<_, AnyError> {
        std::fs::create_dir_all(&db_dir)?;
        Ok(storage_redb::Redb::open(db_dir.join("ethereum.redb"))?)
    })
    .await??;
    let repository = indexing_redb::Repository::new(storage, scope.clone())?;
    let indexer = Arc::new(settings.build(source, repository, None)?);
    let checkpoints: Arc<dyn Checkpoint> = indexer.clone();

    let limits = Limits::new(
        128 * 1024,
        2_000,
        30_000_000,
        Wei::from_u128(1_000_000_000_000),
        Wei::from_u128(100_000_000_000),
        Wei::from_u128(10_000_000_000_000_000_000),
    )?;

    let accounts = Arc::new(AccountClient::new(rpc.clone(), settings.network.chain_id)?);
    let transactions = Arc::new(TransactionClient::new(
        rpc,
        settings.network.chain_id,
        limits,
    )?);
    let coordinator = Arc::new(TransactionCoordinator::new(accounts.clone(), transactions));

    let wallet_config = WalletConfig {
        scope: scope.clone(),
        chain_id: settings.network.chain_id,
        asset: AssetKind::Native,
        decimals: ETH.decimals,
    };

    let provider = WalletProvider::new(
        wallet_config,    
        accounts,
        coordinator,
        indexer,
    );

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
