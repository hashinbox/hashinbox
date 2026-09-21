//! Run with a local Bitcoin Core regtest node:
//! `cargo run --locked -p payment-examples --bin bitcoin_wallet`.
//!
//! The node must be synchronized and unpruned, with `txindex` enabled, RPC
//! available on port 18443, and the `bitcoin`/`bitcoin` RPC credentials.
//! The example opens `.redb-data/bitcoin.redb` and keeps keys in memory only.

use std::{sync::Arc, time::Duration};

use base::BlockPosition;
use chain_bitcoin::{
    AddressType, FeeRate, IndexUtxos, IndexerSettings, KeyPair, Network, RpcClient, WalletConfig,
    WalletProvider,
};
use indexing::Checkpoint;

type AnyError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    let network = Network::Regtest;
    let address_type = AddressType::SegwitV0;

    let rpc_config = json_rpc::Config::new(
        "http://bitcoin:bitcoin@127.0.0.1:18443",
        Duration::from_secs(15),
    );
    let rpc = RpcClient::connect(json_rpc::Http::new(rpc_config)?, network).await?;

    let settings = IndexerSettings::new(network);
    let scope = settings.scope();
    let db_dir = std::path::absolute(".redb-data")?;
    let storage = tokio::task::spawn_blocking(move || -> Result<_, AnyError> {
        std::fs::create_dir_all(&db_dir)?;
        Ok(storage_redb::Redb::open(db_dir.join("bitcoin.redb"))?)
    })
    .await??;
    let repository = Arc::new(indexing_redb::Repository::new(storage, scope.clone())?);
    let indexer = Arc::new(settings.build(rpc.clone(), repository.as_ref().clone(), None)?);
    let checkpoints: Arc<dyn Checkpoint> = indexer.clone();

    let provider = WalletProvider::new(
        WalletConfig {
            scope: scope.clone(),
            network,
            address_type,
            fee_target_blocks: 6,
            max_fee_rate: FeeRate::new(10_000),
        },
        Arc::new(IndexUtxos::new(scope.clone(), network, repository)?),
        Arc::new(rpc.fees()),
        Arc::new(rpc.transactions()),
        indexer,
    );

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
