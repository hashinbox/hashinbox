//! Generate two ETH wallets and watch their mined transactions on local Anvil.
//!
//! Start local Anvil:
//! `anvil --host 127.0.0.1 --chain-id 31337 --hardfork cancun --timestamp 0`
//! Then run `cargo run -p payment-examples --bin ethereum_indexer`.
//!
//! Keys stay in memory; each run generates different wallets. Indexing data lives
//! in `.redb-data/ethereum.redb` relative to the working directory. An existing
//! database resumes its checkpoint; adding filters does not backfill older blocks.

use std::{error::Error, sync::Arc, time::Duration};

use chain_ethereum::{
    AccountClient, AssetKind, ETH, IndexerSettings, Limits, Network, RpcClient, TransactionClient,
    TransactionCoordinator, WalletConfig, WalletProvider, Wei,
};
use indexing::{AddressFilter, BlockPosition, CanonicalAddress, FilterSource};
use indexing_runtime::SyncState;
use tokio::sync::watch;

type AnyError = Box<dyn Error + Send + Sync>;

// linter:disable rust/empty-struct -- Stateless Observer adapter logs committed transactions.
struct TransactionLog;

impl indexing::Observer for TransactionLog {
    fn observed<'a>(&'a self, block: indexing::BlockObservation) -> indexing::BoxFuture<'a, ()> {
        Box::pin(async move {
            for transaction in block.transactions {
                println!("Transaction included: {}", transaction.transaction_id.value);
            }
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    // 1. Configure Ethereum and native ETH wallets.
    let settings = IndexerSettings::new(Network::new(31_337, "anvil"));
    let config = WalletConfig {
        scope: settings.scope(),
        chain_id: settings.network.chain_id,
        asset: AssetKind::Native,
        decimals: ETH.decimals,
    };

    // 2. Share one RPC client and verify identity before opening storage.
    println!("Connecting to Ethereum...");
    let rpc_config = json_rpc::Config::new("http://127.0.0.1:8545", Duration::from_secs(15));
    let rpc = RpcClient::new(json_rpc::Http::new(rpc_config)?);
    let source = settings.source(rpc.clone()).await?;
    println!("Ethereum chain ID verified.");

    // 3. Open the Ethereum indexing database.
    let db_dir = std::path::absolute(".redb-data")?;
    let storage = tokio::task::spawn_blocking(move || -> Result<_, AnyError> {
        std::fs::create_dir_all(&db_dir)?;
        Ok(storage_redb::Redb::open(db_dir.join("ethereum.redb"))?)
    })
    .await??;
    let repository = indexing_redb::Repository::new(storage, settings.scope())?;
    println!("Ethereum indexing database opened.");

    // 4. Create the indexer with a transaction observer.
    let indexer = Arc::new(settings.build(source, repository, Some(Arc::new(TransactionLog)))?);

    // 5. Create the wallet provider with shared account and transaction clients.
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
    let provider: Arc<dyn wallets::Provider> = Arc::new(WalletProvider::new(
        config,
        accounts,
        coordinator,
        indexer.clone(),
    ));

    // 6. Generate and use both wallets through the SDK traits.
    let wallet: Arc<dyn wallets::Wallet> = provider.generate().await?;
    let wallet1: Arc<dyn wallets::Wallet> = provider.generate().await?;
    let address = wallet.address_text(&wallet.address())?;
    let address1 = wallet1.address_text(&wallet1.address())?;
    println!("Wallet address: {}", address.text);
    println!("Wallet-1 address: {}", address1.text);

    // 7. Select both addresses before synchronization starts.
    let selection: Arc<dyn FilterSource> = Arc::new(vec![
        AddressFilter {
            address: CanonicalAddress {
                scope: settings.scope(),
                value: address.text,
            },
            start_position: BlockPosition(0),
        },
        AddressFilter {
            address: CanonicalAddress {
                scope: settings.scope(),
                value: address1.text,
            },
            start_position: BlockPosition(0),
        },
    ]);

    // 8. Run synchronization through the chain-neutral indexing interface.
    let indexer: Arc<dyn indexing::Indexer> = indexer;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (state_tx, mut state_rx) = watch::channel(SyncState::CatchingUp);
    let syncing = indexing_runtime::run(
        indexer,
        selection,
        Duration::from_secs(2),
        shutdown_rx,
        state_tx,
    );
    tokio::pin!(syncing);
    println!("Watching wallet(s). Press Ctrl-C to stop.");

    loop {
        tokio::select! {
            biased;

            result = &mut syncing => {
                result?;
                break;
            }

            signal = tokio::signal::ctrl_c() => {
                signal?;
                shutdown_tx.send_replace(true);
                syncing.await?;
                break;
            }

            changed = state_rx.changed() => {
                changed?;
                println!("Indexer: {:?}", *state_rx.borrow_and_update());
            }
        }
    }

    Ok(())
}
