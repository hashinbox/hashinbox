//! Attach a locally generated Ethereum key to RPC and indexing capabilities.
//!
//! Start local Anvil:
//! `anvil --host 127.0.0.1 --chain-id 31337 --hardfork cancun --timestamp 0`
//! Then run `cargo run --locked -p payment-examples --bin ethereum_wallet`.
//!
//! This opens `.redb-data/ethereum.redb` and constructs an indexer without starting
//! synchronization. Run the `ethereum_indexer` example to watch transactions.
//! The secret stays in memory and is dropped at exit; it is never printed or saved.

type AnyError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    // Sample 1: local key/address only, without a node or database.
    // To run it, uncomment this block and comment out Sample 2 below.
    // {
    //     use base::Addresser;
    //     use chain_ethereum::KeyPair;
    //     use wallets::AddressFormat;
    //
    //     let key = KeyPair::generate()?;
    //     let address = key.address_text(&key.address())?;
    //     println!("Wallet address: {}", address.text);
    // }

    // Sample 2: configure RPC and indexing separately, then attach a local key.
    {
        use std::{sync::Arc, time::Duration};

        use chain_ethereum::{
            AccountClient, AssetKind, ETH, IndexerSettings, KeyPair, Limits, Network, RpcClient,
            TransactionClient, TransactionCoordinator, WalletConfig, WalletProvider, Wei,
        };

        // 1. Construct one shared RPC client from transport configuration.
        let rpc_config = json_rpc::Config::new("http://127.0.0.1:8545", Duration::from_secs(15));
        let rpc = RpcClient::new(json_rpc::Http::new(rpc_config)?);

        // 2. Configure indexing identity and policy; verify before opening storage.
        let settings = IndexerSettings::new(Network::new(31_337, "anvil"));
        println!("Connecting to local Ethereum Anvil...");
        let source = settings.source(rpc.clone()).await?;

        // 3. Open the repository and construct the indexer without synchronization.
        let db_dir = std::path::absolute(".redb-data")?;
        let storage = tokio::task::spawn_blocking(move || -> Result<_, AnyError> {
            std::fs::create_dir_all(&db_dir)?;
            Ok(storage_redb::Redb::open(db_dir.join("ethereum.redb"))?)
        })
        .await??;
        let repository = indexing_redb::Repository::new(storage, settings.scope())?;
        let indexer = Arc::new(settings.build(source, repository, None)?);

        // 4. Compose the provider with shared account, transaction, and history capabilities.
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
                scope: settings.scope(),
                chain_id: settings.network.chain_id,
                asset: AssetKind::Native,
                decimals: ETH.decimals,
            },
            accounts,
            coordinator,
            indexer,
        );

        // 5. Generate the key locally, attach it, and use the shared Wallet traits.
        let key = KeyPair::generate()?;
        let wallet: Arc<dyn wallets::Wallet> = provider.attach(key)?;
        let address = wallet.address_text(&wallet.address())?;
        println!("Wallet address: {}", address.text);
    }

    Ok(())
}
