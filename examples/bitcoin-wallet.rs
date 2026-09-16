//! Attach a local Bitcoin key to a wallet with separately configured RPC and indexing.
//! Run with `cargo run --locked -p payment-examples --bin bitcoin_wallet`.
//!
//! Requires Bitcoin Core 31.x on local regtest, synchronized and unpruned, with
//! synchronized `txindex`, RPC port 18443, and bitcoin/bitcoin RPC credentials.
//! Indexing data lives in `.redb-data/bitcoin.redb`; this sample constructs the
//! indexer and exits without synchronizing. Run `bitcoin_indexer` to watch addresses.
//! The key stays in memory; only the public address is printed.

type AnyError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    // Sample 1: local key/address only, without a node or database.
    // To run it, uncomment this block and comment out Sample 2 below.
    // {
    //     use base::Addresser;
    //     use chain_bitcoin::{AddressType, KeyPair, Network};
    //     use wallets::AddressFormat;
    //
    //     let key = KeyPair::generate(Network::Regtest, AddressType::SegwitV0)?;
    //     let address = key.address_text(&key.address())?;
    //     println!("Wallet address: {}", address.text);
    // }

    // Sample 2: configure RPC and indexing separately, then attach a local key.
    {
        use std::{sync::Arc, time::Duration};

        use chain_bitcoin::{
            AddressType, FeeRate, IndexUtxos, IndexerSettings, KeyPair, Network, RpcClient,
            WalletConfig, WalletProvider,
        };

        let network = Network::Regtest;
        let address_type = AddressType::SegwitV0;

        // 1. Configure transport and verify the node before opening storage.
        let rpc_config = json_rpc::Config::new(
            "http://bitcoin:bitcoin@127.0.0.1:18443",
            Duration::from_secs(15),
        );
        let rpc = RpcClient::connect(json_rpc::Http::new(rpc_config)?, network).await?;

        // 2. Configure indexing independently of the RPC transport.
        let settings = IndexerSettings::new(network);
        let db_dir = std::path::absolute(".redb-data")?;
        let storage = tokio::task::spawn_blocking(move || -> Result<_, AnyError> {
            std::fs::create_dir_all(&db_dir)?;
            Ok(storage_redb::Redb::open(db_dir.join("bitcoin.redb"))?)
        })
        .await??;
        let repository = Arc::new(indexing_redb::Repository::new(storage, settings.scope())?);
        let indexer = Arc::new(settings.build(rpc.clone(), repository.as_ref().clone(), None)?);

        // 3. Share the verified client and indexer with the wallet provider.
        let config = WalletConfig {
            scope: settings.scope(),
            network,
            address_type,
            fee_target_blocks: 6,
            max_fee_rate: FeeRate::new(10_000),
        };
        let utxos = Arc::new(IndexUtxos::new(settings.scope(), network, repository)?);
        let provider = WalletProvider::new(
            config,
            utxos,
            Arc::new(rpc.fees()),
            Arc::new(rpc.transactions()),
            indexer,
        );

        // 4. Generate the key locally, then attach the operational capabilities.
        let key = KeyPair::generate(network, address_type)?;
        let wallet: Arc<dyn wallets::Wallet> = provider.attach(key)?;
        let address = wallet.address_text(&wallet.address())?;
        println!("Wallet address: {}", address.text);
    }

    Ok(())
}
