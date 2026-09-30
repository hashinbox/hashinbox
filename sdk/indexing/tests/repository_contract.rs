//! Contract coverage for the two shipped stores.
//!
//! The shared `contracts::{BackfillContract, BalanceContract, HistoryContract,
//! PurgeContract, SpendContract, WatchContract}` scenarios run
//! here, and each one runs **twice**: once against `indexing::memory::Repository`
//! on maps and once against `indexing::sqlite::Repository` on a fresh file. Two
//! implementations, one set of scenarios, so they stay a statement about the
//! ports rather than about either store — and so a divergence between the two
//! is a failing test rather than a surprise. It has been one: a blank address
//! that the map-backed store refused and the file answered with an empty page.
//!
//! Everything else here is what a store on a *file* owes the ports and a store
//! in memory cannot be asked: a reopen, and a coordinate that survives one.

mod contracts;

use std::path::Path;

use base::Decimal;
use indexing::{memory, sqlite};

use contracts::{
    BackfillContract, BalanceContract, HistoryContract, PurgeContract, SpendContract, WatchContract,
};
use indexing::{
    Address, AssetId, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, CanonicalBlock,
    ChainName, ConfirmedHistory, HistoryQuery, IndexErrorKind, IndexStore, IndexedOutput,
    InterpretedBlock, MovementId, Network, ObservationDraft, ObservationDraftStatus, OutputChanges,
    OutputId, OutputRequest, OutputStore, TransactionRef, ValueMovement,
};
use tempfile::TempDir;

/// SQLite work runs on `tokio::task::spawn_blocking`, so these scenarios need
/// a Tokio runtime rather than a bare executor.
fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .build()
                .expect("test runtime")
        })
        .block_on(future)
}

fn network() -> Network {
    Network {
        chain: ChainName::new("chain"),
        name: "testnet".into(),
    }
}

fn open(path: &Path) -> sqlite::Repository {
    let storage = sqlite::Database::open(path).expect("temporary sqlite database");
    sqlite::Repository::new(storage)
}

fn block(height: u64, hash: u8, parent: u8) -> BlockRef {
    BlockRef {
        position: BlockPosition(height),
        height: BlockHeight(height),
        hash: BlockHash(vec![hash]),
        parent: Some(BlockParent {
            position: BlockPosition(height - 1),
            hash: BlockHash(vec![parent]),
        }),
        timestamp: Some(1_000 + height),
    }
}

fn sparse_block(
    position: u64,
    height: u64,
    hash: u8,
    parent_position: u64,
    parent_hash: u8,
) -> BlockRef {
    BlockRef {
        position: BlockPosition(position),
        height: BlockHeight(height),
        hash: BlockHash(vec![hash]),
        parent: Some(BlockParent {
            position: BlockPosition(parent_position),
            hash: BlockHash(vec![parent_hash]),
        }),
        timestamp: Some(1_000 + position),
    }
}

fn address(value: &str) -> Address {
    Address {
        network: network(),
        value: value.into(),
    }
}

fn transaction(value: &str) -> TransactionRef {
    TransactionRef {
        network: network(),
        id: value.into(),
    }
}

fn draft(id: &str) -> ObservationDraft {
    ObservationDraft {
        transaction_id: transaction(id),
        transaction_order: 0,
        status: ObservationDraftStatus::Included,
        movements: vec![ValueMovement::transfer(
            MovementId(format!("movement-{id}")),
            AssetId {
                chain: network().chain,
                asset: "native".into(),
            },
            Decimal::from(1_u64),
            address("sender"),
            address("receiver"),
        )],
        fee: None,
    }
}

fn output(id: &str) -> IndexedOutput {
    IndexedOutput {
        id: OutputId {
            transaction: transaction(id),
            index: 0,
        },
        address: address("receiver"),
        asset: AssetId {
            chain: network().chain,
            asset: "native".into(),
        },
        amount: Decimal::from(1_u64),
        evidence: vec![1, 2, 3],
        created_at: BlockHeight(1),
        coinbase: false,
    }
}

fn addition(
    block: BlockRef,
    expected: Option<BlockRef>,
    transaction: &str,
    outputs: OutputChanges,
) -> indexing::IndexedBatch {
    let canonical = CanonicalBlock::new(
        network(),
        InterpretedBlock {
            outputs,
            ..InterpretedBlock::new(&network(), block, vec![draft(transaction)])
                .expect("unique transaction ids")
        },
    )
    .expect("valid block facts");
    indexing::IndexedBatch::new(
        network(),
        expected,
        vec![canonical],
        Vec::new(),
        indexing::FeeSettlement::Charged,
    )
    .expect("a connected block")
}

fn tip(repository: &sqlite::Repository) -> Option<BlockRef> {
    block_on(repository.checkpoint(&network())).expect("checkpoint")
}

fn history(repository: &sqlite::Repository) -> Vec<String> {
    block_on(ConfirmedHistory::history(
        repository,
        HistoryQuery {
            address: address("receiver"),
            asset: indexing::AssetId::native(network().chain.clone()),
            after: None,
            limit: 10,
        },
    ))
    .expect("history")
    .transactions
    .into_iter()
    .map(|transaction| transaction.transaction_id.id.to_string())
    .collect()
}

fn outputs(repository: &sqlite::Repository) -> Vec<IndexedOutput> {
    block_on(OutputStore::list(
        repository,
        OutputRequest {
            network: network(),
            address: address("receiver"),
            after: None,
            limit: 10,
        },
    ))
    .expect("outputs")
    .outputs
}

#[test]
fn duplicate_output_identity_is_rejected_before_repository_add() {
    let directory = TempDir::new().expect("temporary directory");
    let database = directory.path().join("index.db");
    let repository = open(&database);
    let first = output("duplicate");
    let mut second = first.clone();
    second.address = address("another-receiver");

    let error = CanonicalBlock::new(
        network(),
        InterpretedBlock {
            outputs: OutputChanges {
                created: vec![first, second],
                ..OutputChanges::default()
            },
            ..InterpretedBlock::new(&network(), block(1, 1, 0), vec![draft("duplicate")])
                .expect("unique transaction ids")
        },
    )
    .expect_err("duplicate output identity");

    assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
    assert_eq!(tip(&repository), None);
}

#[test]
fn history_pages_survive_a_moving_checkpoint_and_output_pages_do_not() {
    let directory = TempDir::new().expect("temporary directory");
    let repository = open(&directory.path().join("index.db"));
    let first = block(1, 1, 0);
    let second = block(2, 2, 1);
    let first_output = output("one");
    let mut second_output = output("two");
    second_output.created_at = second.height;
    for (block, expected, id, output) in [
        (first.clone(), None, "one", first_output.clone()),
        (second.clone(), Some(first), "two", second_output.clone()),
    ] {
        block_on(repository.commit(addition(
            block,
            expected,
            id,
            OutputChanges {
                created: vec![output],
                ..OutputChanges::default()
            },
        )))
        .expect("seed block");
    }
    let history_page = |after| {
        block_on(ConfirmedHistory::history(
            &repository,
            HistoryQuery {
                address: address("receiver"),
                asset: indexing::AssetId::native(network().chain.clone()),
                after,
                limit: 1,
            },
        ))
    };
    let output_page = |after| {
        block_on(OutputStore::list(
            &repository,
            OutputRequest {
                network: network(),
                address: address("receiver"),
                after,
                limit: 1,
            },
        ))
    };

    let page = history_page(None).expect("first history page");
    assert_eq!(page.checkpoint, Some(second.clone()));
    assert_eq!(page.transactions.len(), 1);
    assert_eq!(page.transactions[0].transaction_id, transaction("one"));
    let history_cursor = page.next.expect("history continuation");
    let page = history_page(Some(history_cursor.clone())).expect("final history page");
    assert_eq!(page.checkpoint, Some(second.clone()));
    assert_eq!(page.transactions.len(), 1);
    assert_eq!(page.transactions[0].transaction_id, transaction("two"));
    assert_eq!(page.next, None);

    let page = output_page(None).expect("first output page");
    assert_eq!(page.checkpoint, Some(second.clone()));
    assert_eq!(page.outputs, vec![first_output]);
    let output_cursor = page.next.expect("output continuation");
    let page = output_page(Some(output_cursor.clone())).expect("final output page");
    assert_eq!(page.checkpoint, Some(second.clone()));
    assert_eq!(page.outputs, vec![second_output]);
    assert_eq!(page.next, None);

    block_on(repository.commit(addition(
        block(3, 3, 2),
        Some(second),
        "three",
        OutputChanges::default(),
    )))
    .expect("advance checkpoint");
    // Confirmed history is append-only, so a checkpoint that moved can only
    // have appended above the cursor: the page is the same one.
    let page = history_page(Some(history_cursor)).expect("an ascending cursor stays valid");
    assert_eq!(page.transactions.len(), 1);
    assert_eq!(page.transactions[0].transaction_id, transaction("two"));
    let page = history_page(page.next).expect("the appended row is simply the next page");
    assert_eq!(page.transactions[0].transaction_id, transaction("three"));
    assert_eq!(page.next, None);
    // The live-output set is not append-only — a spend removes a row — so its
    // cursor is still bound to the checkpoint it was issued at.
    let error = output_page(Some(output_cursor)).expect_err("stale output cursor");
    assert_eq!(error.kind, IndexErrorKind::Conflict);
    assert!(error.retryable);
    assert_eq!(error.message, "outputs changed during pagination");
}

/// A re-created output and a stale checkpoint both leave every stored byte
/// untouched, and a restart finds exactly what was committed. An unmatched
/// required spend is deliberately not one of the refusals: it commits and is
/// reported instead (docs/design/024 §5.1, `SpendContract`).
///
/// The two refusals are different kinds on purpose: a stale checkpoint is a
/// retryable `Conflict`, because the pass simply reads the line again, while an
/// output re-created under different contents is a terminal `Store` failure,
/// because nothing about reading again would change it.
#[test]
fn restart_and_refusals_preserve_atomic_canonical_state() {
    let directory = TempDir::new().expect("temporary directory");
    let database = directory.path().join("index.db");
    let repository = open(&database);
    let first = block(1, 1, 0);
    let funding = output("funding");
    assert_eq!(
        block_on(repository.commit(addition(
            first.clone(),
            None,
            "funding",
            OutputChanges {
                created: vec![funding.clone()],
                ..OutputChanges::default()
            },
        )))
        .expect("first block"),
        Vec::new(),
        "nothing was spent, so no required spend went unmatched",
    );

    let second = block(2, 2, 1);
    let mut recreated = funding.clone();
    recreated.created_at = BlockHeight(2);
    let invalid = block_on(repository.commit(addition(
        second.clone(),
        Some(first.clone()),
        "recreated-output",
        OutputChanges {
            created: vec![recreated],
            ..OutputChanges::default()
        },
    )))
    .expect_err("re-created live output");
    // Terminal, not retryable: one `(address, outpoint)` cannot have two
    // contents, and reading the block again cannot change what the file holds.
    assert_eq!(invalid.kind, IndexErrorKind::Store);
    assert!(!invalid.retryable);
    assert_eq!(tip(&repository), Some(first.clone()));
    assert_eq!(history(&repository), vec!["funding"]);
    assert_eq!(outputs(&repository), vec![funding.clone()]);

    let stale = block_on(repository.commit(addition(
        second.clone(),
        None,
        "stale",
        OutputChanges::default(),
    )))
    .expect_err("stale checkpoint");
    assert_eq!(stale.kind, IndexErrorKind::Conflict);
    assert_eq!(tip(&repository), Some(first.clone()));
    assert_eq!(history(&repository), vec!["funding"]);

    block_on(repository.commit(addition(
        second.clone(),
        Some(first.clone()),
        "spend",
        OutputChanges {
            spent: vec![funding.key()],
            ..OutputChanges::default()
        },
    )))
    .expect("second block");
    assert_eq!(history(&repository), vec!["funding", "spend"]);
    assert!(outputs(&repository).is_empty());

    drop(repository);
    let repository = open(&database);
    assert_eq!(tip(&repository), Some(second));
    assert_eq!(history(&repository), vec!["funding", "spend"]);
    assert!(outputs(&repository).is_empty());
}

#[test]
fn sparse_coordinates_round_trip_through_restart() {
    let directory = TempDir::new().expect("temporary directory");
    let database = directory.path().join("sparse-index.db");
    let repository = open(&database);
    let first = sparse_block(100, 50, 1, 97, 0);
    let second = sparse_block(103, 51, 2, 100, 1);

    block_on(repository.commit(addition(
        first.clone(),
        None,
        "sparse-first",
        OutputChanges::default(),
    )))
    .expect("first sparse block");
    block_on(repository.commit(addition(
        second.clone(),
        Some(first.clone()),
        "sparse-second",
        OutputChanges::default(),
    )))
    .expect("second sparse block");

    drop(repository);
    let repository = open(&database);
    assert_eq!(tip(&repository), Some(second.clone()));

    let page = block_on(ConfirmedHistory::history(
        &repository,
        HistoryQuery {
            address: address("receiver"),
            asset: indexing::AssetId::native(network().chain.clone()),
            after: None,
            limit: 10,
        },
    ))
    .expect("sparse history");
    let blocks: Vec<BlockRef> = page
        .transactions
        .into_iter()
        .map(|transaction| transaction.block().clone())
        .collect();
    assert_eq!(blocks, [first, second.clone()]);

    drop(repository);
    assert_eq!(tip(&open(&database)), Some(second));
}

/// Runs one shared scenario against **both** shipped stores, as two tests under
/// the scenario's own name.
///
/// Pairing them here rather than in each backend's own modules is what makes a
/// disagreement legible: the two tests differ in one line, so a scenario only a
/// file satisfies — or only maps do — names both halves at once.
macro_rules! contract {
    ($contract:ident, $($scenario:ident),* $(,)?) => {$(
        mod $scenario {
            use super::*;

            #[test]
            fn memory() {
                let store = memory::Repository::new();
                block_on($contract::new(&store, network()).$scenario());
            }

            #[test]
            fn sqlite() {
                let directory = TempDir::new().expect("temporary directory");
                let store = open(&directory.path().join(concat!(stringify!($scenario), ".db")));
                block_on($contract::new(&store, network()).$scenario());
            }
        }
    )*};
}

contract!(
    HistoryContract,
    limit_counts_only_the_query_asset,
    returns_only_query_asset_movements,
    requires_the_address_as_endpoint,
    failed_fee_payment_appears_for_every_asset,
    successful_fee_payment_appears_for_the_fee_asset_only,
    cursor_resumes_across_assets,
    pages_equal_the_selected_history,
    a_cursor_survives_writes_that_land_after_it,
    pages_run_in_native_block_order,
    an_ordinal_is_the_blocks_own_under_every_selection,
    a_page_claims_a_whole_history_only_once_it_has_been_read,
    a_page_born_above_the_first_block_claims_no_whole_history,
    an_overdrawn_fold_does_not_reach_the_page_of_the_same_rows,
    a_whole_history_is_not_claimed_below_the_line_it_is_read_at,
    a_sibling_still_catching_up_weakens_every_read_of_the_address,
    only_what_the_address_owes_below_the_line_weakens_its_reads,
    a_whole_history_is_not_claimed_before_the_first_commit,
);

contract!(
    BalanceContract,
    folds_the_movements_of_one_holding,
    never_charges_a_settled_fee_twice,
    charges_a_fee_to_the_fee_asset_only,
    reports_an_overdrawn_fold_as_incomplete,
    folds_a_credit_no_transaction_names,
    an_unindexed_holding_is_zero,
    refuses_a_holding_it_cannot_own,
    an_observed_total_starts_from_the_rows_already_stored,
    a_repeated_commit_moves_an_observed_total_by_nothing,
    an_observed_deficit_reads_as_overdrawn,
    a_movement_named_like_a_fee_still_commits,
    a_birthday_at_the_first_block_claims_nothing_uninspected,
);

contract!(
    BackfillContract,
    fills_history_below_the_checkpoint,
    resumes_where_an_interrupted_batch_stopped,
    redoes_a_batch_without_duplicating_it,
    fills_in_a_credit_no_transaction_names,
    leaves_no_output_the_batch_itself_spent,
    refuses_a_batch_whose_checkpoint_moved,
    refuses_a_claim_that_expected_other_progress,
    refuses_a_claim_for_an_unknown_observation,
    refuses_a_batch_whole_when_one_block_disagrees,
    refuses_a_row_that_disagrees_with_the_stored_one,
    writes_what_a_block_at_the_line_carries_beyond_the_stored_facts,
    refuses_a_block_at_the_line_whose_facts_disagree,
    records_progress_for_its_own_network_only,
    refuses_a_batch_without_moving_the_balance_or_the_outputs,
    forgets_one_observations_progress_and_keeps_its_rows,
    offers_no_output_until_the_range_is_read,
);

contract!(
    WatchContract,
    round_trips_every_field,
    rejects_a_redefined_identity,
    accepts_one_address_once_per_asset,
    preserves_progress_on_an_exact_repeat,
    writes_a_batch_whole_or_not_at_all,
    refuses_a_batch_of_two_networks,
    refuses_a_blank_address,
    forgets_an_identity_and_accepts_it_again,
    keeps_the_progress_of_each_observation_apart,
    reanchors_an_observation_and_discards_its_claim,
    reanchors_an_observation_the_store_has_lost,
    refuses_a_reanchor_below_the_stored_birthday,
    reanchoring_keeps_what_the_observation_already_counted,
    refuses_a_reanchor_onto_a_pair_another_identity_holds,
    answers_only_its_own_network,
);

contract!(
    SpendContract,
    an_unknown_required_spend_is_reported_not_refused,
    a_required_spend_of_another_owner_is_reported,
    a_matched_spend_reports_nothing,
    a_tracked_spend_is_never_reported,
    a_reported_miss_commits_the_rest_of_its_block,
    a_reported_miss_outlives_the_block_that_reported_it,
    a_backfilled_output_is_spent_by_the_next_block,
    a_recreated_output_is_still_an_error,
    a_replayed_creating_block_never_resurrects_a_spent_output,
    an_output_page_names_a_successor_only_when_one_exists,
);

/// The offline rescan over one store two networks share
/// (`contracts::PurgeContract`), on both backends.
///
/// Outside `contract!` because this contract is constructed with a second
/// network rather than with one.
mod purge_network_removes_only_its_own_network {
    use super::*;

    fn elsewhere() -> Network {
        Network::new(ChainName::new("other"), "testnet")
    }

    #[test]
    fn memory() {
        let store = memory::Repository::new();
        block_on(
            PurgeContract::new(&store, network(), elsewhere())
                .purge_network_removes_only_its_own_network(),
        );
    }

    #[test]
    fn sqlite() {
        let directory = TempDir::new().expect("temporary directory");
        let store = open(&directory.path().join("purge.db"));
        block_on(
            PurgeContract::new(&store, network(), elsewhere())
                .purge_network_removes_only_its_own_network(),
        );
    }
}
