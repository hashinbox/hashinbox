//! Uncomment a sample call in `main`, then run:
//! `cargo run --locked -p payment-examples --bin wallets_collections`.
//!
//! Bitcoin requires Core 31.x on local regtest, synchronized and unpruned, with
//! synchronized `txindex`, RPC port 18443, and bitcoin/bitcoin RPC credentials.
//! Ethereum requires local Anvil:
//! `anvil --host 127.0.0.1 --chain-id 31337 --hardfork cancun --timestamp 0`.
//! Samples use separate redb files under `.redb-data` without synchronization.
//! Keys and wallet registrations stay in memory and are dropped at exit.
//! Output contains only public wallet metadata and indexing filters.

use std::{sync::Arc, time::Duration};

use indexing::{Checkpoint, IndexScope};

type AnyError = Box<dyn std::error::Error + Send + Sync>;

/// The application selects a configured family independently of each wallet ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum WalletAsset {
    Btc,
    Eth,
}

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    // bitcoin_wallet_collections().await?;
    // ethereum_wallet_collections().await?;

    Ok(())
}

#[allow(
    dead_code,
    reason = "Enable this sample by uncommenting its call in main."
)]
async fn bitcoin_wallet_collections() -> Result<(), AnyError> {
    // 1. Compose the concrete Bitcoin dependencies once.
    let (provider, checkpoints, scope) = connect_bitcoin_provider().await?;
    let sender = provider.transactions();

    // 2. Register the BTC family. This does not create a wallet.
    let mut wallets = wallets::Wallets::<&str, WalletAsset>::new(checkpoints);
    wallets.register(WalletAsset::Btc, scope, provider, sender, None)?;
    println!("Registered BTC family; no wallets created yet.");

    // 3. Create two wallets through the same registered provider.
    // Each result is WalletInfo; the collection retains the operational Wallet.
    let alice = wallets.generate("alice", &WalletAsset::Btc).await?;
    let bob = wallets.generate("bob", &WalletAsset::Btc).await?;
    for info in [&alice, &bob] {
        println!(
            "Generated {} ({:?}): {}",
            info.id, info.family, info.address.text
        );
    }

    // 4. Look up public metadata by wallet ID, not by asset family.
    let found = wallets.get("alice")?;
    println!("Lookup {}: {}", found.id, found.address.text);

    // 5. Inspect the address/birthday selection that an indexer would consume.
    // No synchronization runs here, so balance/history are not demonstrated.
    let filters = wallets.filters()?;
    println!("Indexing filters: {}", filters.len());
    for filter in filters {
        println!(
            "Selected address: {}, start position: {}",
            filter.address.value, filter.start_position.0
        );
    }

    Ok(())
}

#[allow(
    dead_code,
    reason = "Enable this sample by uncommenting its call in main."
)]
async fn ethereum_wallet_collections() -> Result<(), AnyError> {
    // 1. Compose the concrete Ethereum dependencies once.
    let (provider, checkpoints, scope) = connect_ethereum_provider().await?;
    let sender = provider.transactions();

    // 2. Register the native ETH family. This does not create a wallet.
    let mut wallets = wallets::Wallets::<&str, WalletAsset>::new(checkpoints);
    wallets.register(WalletAsset::Eth, scope, provider, sender, None)?;
    println!("Registered ETH family; no wallets created yet.");

    // 3. Create two wallets through the same registered provider.
    // Each result is WalletInfo; the collection retains the operational Wallet.
    let alice = wallets.generate("alice", &WalletAsset::Eth).await?;
    let bob = wallets.generate("bob", &WalletAsset::Eth).await?;
    for info in [&alice, &bob] {
        println!(
            "Generated {} ({:?}): {}",
            info.id, info.family, info.address.text
        );
    }

    // 4. Look up public metadata by wallet ID, not by asset family.
    let found = wallets.get("alice")?;
    println!("Lookup {}: {}", found.id, found.address.text);

    // 5. Inspect the address/birthday selection that an indexer would consume.
    // No synchronization runs here, so balance/history are not demonstrated.
    let filters = wallets.filters()?;
    println!("Indexing filters: {}", filters.len());
    for filter in filters {
        println!(
            "Selected address: {}, start position: {}",
            filter.address.value, filter.start_position.0
        );
    }

    Ok(())
}

async fn connect_bitcoin_provider() -> Result<
    (
        chain_bitcoin::WalletProvider,
        Arc<dyn Checkpoint>,
        IndexScope,
    ),
    AnyError,
> {
    use chain_bitcoin::{
        AddressType, FeeRate, IndexUtxos, IndexerSettings, Network, RpcClient, WalletConfig,
        WalletProvider,
    };

    let network = Network::Regtest;
    let rpc_config = json_rpc::Config::new(
        "http://bitcoin:bitcoin@127.0.0.1:18443",
        Duration::from_secs(15),
    );
    let rpc = RpcClient::connect(json_rpc::Http::new(rpc_config)?, network).await?;
    let settings = IndexerSettings::new(network);
    let scope = settings.scope();
    let storage = open_storage("wallets-collections.redb").await?;
    let repository = Arc::new(indexing_redb::Repository::new(storage, scope.clone())?);
    let indexer = Arc::new(settings.build(rpc.clone(), repository.as_ref().clone(), None)?);
    let utxos = Arc::new(IndexUtxos::new(scope.clone(), network, repository)?);
    let provider = WalletProvider::new(
        WalletConfig {
            scope: scope.clone(),
            network,
            address_type: AddressType::SegwitV0,
            fee_target_blocks: 6,
            max_fee_rate: FeeRate::new(10_000),
        },
        utxos,
        Arc::new(rpc.fees()),
        Arc::new(rpc.transactions()),
        indexer.clone(),
    );
    Ok((provider, indexer, scope))
}

async fn connect_ethereum_provider() -> Result<
    (
        chain_ethereum::WalletProvider,
        Arc<dyn Checkpoint>,
        IndexScope,
    ),
    AnyError,
> {
    use chain_ethereum::{
        AccountClient, AssetKind, ETH, IndexerSettings, Limits, Network, RpcClient,
        TransactionClient, TransactionCoordinator, WalletConfig, WalletProvider, Wei,
    };

    let rpc_config = json_rpc::Config::new("http://127.0.0.1:8545", Duration::from_secs(15));
    let rpc = RpcClient::new(json_rpc::Http::new(rpc_config)?);
    let settings = IndexerSettings::new(Network::new(31_337, "anvil"));
    let scope = settings.scope();
    let source = settings.source(rpc.clone()).await?;
    let storage = open_storage("wallets-collections-ethereum.redb").await?;
    let repository = indexing_redb::Repository::new(storage, scope.clone())?;
    let indexer = Arc::new(settings.build(source, repository, None)?);
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

async fn open_storage(file_name: &str) -> Result<storage_redb::Redb, AnyError> {
    let db_dir = std::path::absolute(".redb-data")?;
    let db_path = db_dir.join(file_name);
    tokio::task::spawn_blocking(move || -> Result<_, AnyError> {
        std::fs::create_dir_all(&db_dir)?;
        Ok(storage_redb::Redb::open(db_path)?)
    })
    .await?
}
