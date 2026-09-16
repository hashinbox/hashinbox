//! Generate Bitcoin wallets and watch their indexed transactions on local regtest.
//! Run with `cargo run -p payment-examples --bin bitcoin_indexer`.

use chain_bitcoin::{AddressType, FeeRate, IndexerSettings, Network, WalletConfig, WalletProvider};
use indexing::{ChainId, IndexScope};
use std::{error::Error, sync::Arc, time::Duration};

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

const GENESIS_HASH: &str = "0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206";

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    // 1. configure the Bitcoin wallet
    let network = Network::Regtest;

    let config = WalletConfig {
        scope: IndexScope {
            chain: ChainId(chain_bitcoin::CHAIN.to_owned()),
            network: network.canonical_name().to_owned(),
        },
        network,
        address_type: AddressType::SegwitV0,
        fee_target_blocks: 6,
        max_fee_rate: FeeRate::new(10_000),
    };

    // 2.Create the Bitcoin RPC client
    let settings = IndexerSettings::new(
        "http://bitcoin:bitcoin@127.0.0.1:18443".to_owned(),
        network,
        GENESIS_HASH,
    );

    let rpc = settings.client().await?;

    // 3. Open an embedded redb database and create the Bitcoin indexing repository.
    let db_dir = std::path::absolute(".redb-data")?;
    std::fs::create_dir_all(&db_dir)?;
    let db_path = db_dir.join("bitcoin.redb");
    let storage = tokio::task::spawn_blocking(|| storage_redb::Redb::open(db_path)).await??;

    let repository = Arc::new(indexing_redb::Repository::new(storage, settings.scope())?);

    // 4. Create the Bitcoin indexer
    let indexer = Arc::new(settings.build(
        rpc.clone(),
        repository.as_ref().clone(),
        Some(Arc::new(TransactionLog)),
    )?);

    // 5. Create the wallet provider.
    let utxos = Arc::new(chain_bitcoin::IndexUtxos::new(
        config.scope.clone(),
        network,
        repository.clone(),
    )?);

    let provider: Arc<dyn wallets::Provider> = Arc::new(WalletProvider::new(
        config,
        utxos,
        Arc::new(rpc.fees()),
        Arc::new(rpc.transactions()),
        indexer.clone(),
    ));

    // 6. Generate and use the wallet through the SDK trait.
    let wallet: Arc<dyn wallets::Wallet> = provider.generate().await?;
    let wallet1: Arc<dyn wallets::Wallet> = provider.generate().await?;

    let address = wallet.address_text(&wallet.address())?;
    let address1 = wallet1.address_text(&wallet1.address())?;
    println!("Wallet address: {}", address.text);
    println!("Wallet-1 address: {}", address1.text);

    // 7. Select the generated wallet's address.
    let selection: Arc<dyn indexing::FilterSource> = Arc::new(vec![
        indexing::AddressFilter {
            address: indexing::CanonicalAddress {
                scope: settings.scope(),
                value: address.text,
            },
            start_position: indexing::BlockPosition(0),
        },
        indexing::AddressFilter {
            address: indexing::CanonicalAddress {
                scope: settings.scope(),
                value: address1.text,
            },
            start_position: indexing::BlockPosition(0),
        },
    ]);

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let (state_tx, mut state_rx) =
        tokio::sync::watch::channel(indexing_runtime::SyncState::CatchingUp);

    // 8. Run synchronization and report its status.
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
