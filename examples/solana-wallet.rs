//! Generate two native SOL wallets and watch finalized localnet transactions.
//!
//! Use a local validator at http://127.0.0.1:8899 with the Memo-v3 program loaded.
//! Obtain its genesis hash with `solana --url http://127.0.0.1:8899 genesis-hash`.
//! Run `cargo run -p payment-examples --bin solana_wallet -- <GENESIS_HASH>`.
//! Send SOL to a printed address; the observer reports it after finalization.
//!
//! Keys stay in memory, so each run generates different wallets. New wallets
//! are watched from the finalized slot read before generation. Indexing uses
//! `.redb-data/solana-<GENESIS_HASH>.redb` relative to the working directory:
//! different local ledgers get different files, and the same ledger resumes its
//! checkpoint. The validator must retain that checkpoint and subsequent blocks.
//! This example generates and watches wallets; it does not submit transfers.

use std::{error::Error, sync::Arc, time::Duration};

use chain_solana::{
    AssetKind, BlockInterpreter, Coordinator, GenesisHash, RegistrationError, RpcClient,
    RpcCommitment, RpcConfig, Source, SubmissionRegistrar, SubmissionTask, WalletConfig,
    WalletProvider,
};
use indexing::{AddressFilter, BlockPosition, CanonicalAddress, FilterSource};
use indexing_runtime::SyncState;
use tokio::sync::{mpsc, watch};

type AnyError = Box<dyn Error + Send + Sync>;

struct TransactionLog {
    progress: watch::Sender<Option<indexing::BlockRef>>,
}

impl indexing::Observer for TransactionLog {
    fn observed<'a>(&'a self, block: indexing::BlockObservation) -> indexing::BoxFuture<'a, ()> {
        Box::pin(async move {
            self.progress.send_replace(Some(block.block));
            for transaction in block.transactions {
                println!(
                    "Finalized transaction: {}",
                    transaction.transaction_id.value
                );
            }
        })
    }
}

// The coordinator registers work before dispatch. Main owns this bounded queue
// and runs every accepted task, keeping indexing alive until they finish.
struct Submissions {
    pending: mpsc::Sender<SubmissionTask>,
}

impl SubmissionRegistrar for Submissions {
    fn register<'a>(
        &'a self,
        task: SubmissionTask,
    ) -> indexing::BoxFuture<'a, Result<(), RegistrationError>> {
        Box::pin(async move {
            self.pending
                .send(task)
                .await
                .map_err(|_| RegistrationError::Closed)
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    // 1. Configure one local ledger and native SOL wallets.
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let [genesis] = arguments.as_slice() else {
        return Err("Usage: solana_wallet <GENESIS_HASH>".into());
    };
    let genesis: GenesisHash = genesis.parse()?;
    let config = WalletConfig::new("localnet", AssetKind::Native)?;
    let scope = config.scope().clone();
    let rpc = RpcClient::connect(RpcConfig::new(
        "http://127.0.0.1:8899",
        Duration::from_secs(30),
        1024 * 1024,      // Maximum request bytes.
        64 * 1024 * 1024, // Maximum response bytes for full blocks.
    )?)?;

    // 2. Verify identity and Memo availability before opening storage.
    println!("Connecting to Solana...");
    rpc.verify_genesis(&genesis).await?;
    rpc.verify_memo().await?;
    let birthday = BlockPosition(rpc.slot(RpcCommitment::Finalized, None).await?);
    println!(
        "Solana genesis and Memo-v3 verified; starting slot: {}",
        birthday.0
    );

    // 3. Open this ledger's indexing database off the async executor.
    let db_dir = std::path::absolute(".redb-data")?;
    let db_path = db_dir.join(format!("solana-{genesis}.redb"));
    let storage = tokio::task::spawn_blocking(move || -> Result<_, AnyError> {
        std::fs::create_dir_all(&db_dir)?;
        Ok(storage_redb::Redb::open(db_path)?)
    })
    .await??;
    let repository = indexing_redb::Repository::new(storage, scope.clone())?;
    println!("Solana indexing database opened.");

    // 4. Compose the sparse-slot source and interpreter with the shared indexer.
    let mut service = indexing::Service::new(
        Source::new(rpc.clone()),
        BlockInterpreter::new(scope.clone())?,
        repository,
        indexing::SyncConfig::new(scope.clone(), 1, 128, 32)?,
    );
    let checkpoint = indexing::Checkpoint::checkpoint(&service, &scope).await?;
    let (progress, progress_rx) = watch::channel(checkpoint);
    service.observe(Arc::new(TransactionLog { progress }));
    let indexer = Arc::new(service);

    // 5. Give the provider the real SDK sender and application-owned supervision.
    let (submissions, mut pending) = mpsc::channel(16);
    let coordinator = Arc::new(Coordinator::new(
        rpc.clone(),
        Arc::new(Submissions {
            pending: submissions,
        }),
        indexer.clone(),
        indexer.clone(),
        scope.clone(),
        progress_rx,
    ));
    let provider: Arc<dyn wallets::Provider> = Arc::new(WalletProvider::new(
        config,
        rpc,
        indexer.clone(),
        coordinator,
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
                scope: scope.clone(),
                value: address.text,
            },
            start_position: birthday,
        },
        AddressFilter {
            address: CanonicalAddress {
                scope: scope.clone(),
                value: address1.text,
            },
            start_position: birthday,
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

    let mut tasks = tokio::task::JoinSet::new();
    let mut stopping = false;
    loop {
        if stopping && pending.is_empty() && tasks.is_empty() {
            shutdown_tx.send_replace(true);
            syncing.await?;
            break;
        }
        tokio::select! {
            biased;

            result = &mut syncing => {
                result?;
                break;
            }

            signal = tokio::signal::ctrl_c(), if !stopping => {
                signal?;
                pending.close();
                stopping = true;
                println!("Finishing submissions before stopping the indexer...");
            }

            Some(task) = pending.recv(), if tasks.len() < 16 => {
                tasks.spawn(task.run());
            }

            Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                result?;
            }

            changed = state_rx.changed() => {
                changed?;
                println!("Indexer: {:?}", *state_rx.borrow_and_update());
            }
        }
    }

    Ok(())
}
