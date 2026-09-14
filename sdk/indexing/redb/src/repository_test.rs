use std::ops::Deref;

use base::Decimal;
use futures_executor::block_on;
use indexing::{
    AssetId, BlockAddition, BlockHash, BlockHeight, BlockOutcome, BlockParent, BlockPosition,
    BlockRef, BlockSelector, Blocks, CanonicalAddress, ChainId, HistoryQuery, IndexScope,
    IndexedOutput, InterpretedBlock, MovementId, NetworkFee, ObservationDraft,
    ObservationDraftStatus, OutputChanges, OutputId, OutputRequest, Outputs, TransactionRef,
    Transactions, ValueMovement,
};
use tempfile::TempDir;

use super::Repository;

struct TestRepository {
    repository: Repository,
    _directory: TempDir,
}

impl Deref for TestRepository {
    type Target = Repository;

    fn deref(&self) -> &Self::Target {
        &self.repository
    }
}

fn scope() -> IndexScope {
    IndexScope {
        chain: ChainId("chain".to_owned()),
        network: "test".to_owned(),
    }
}

fn repository() -> TestRepository {
    let directory = TempDir::new().expect("temporary database directory");
    let database = directory.path().join("index.redb");
    let storage = storage_redb::Redb::open(&database).expect("temporary database");
    TestRepository {
        repository: Repository::new(storage, scope()).expect("repository scope"),
        _directory: directory,
    }
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

fn address(value: &str) -> CanonicalAddress {
    CanonicalAddress {
        scope: scope(),
        value: value.to_owned(),
    }
}

fn transaction(value: &str) -> TransactionRef {
    TransactionRef {
        scope: scope(),
        value: value.to_owned(),
    }
}

fn draft(id: &str) -> ObservationDraft {
    ObservationDraft {
        scope: scope(),
        transaction_id: transaction(id),
        status: ObservationDraftStatus::Included,
        movements: vec![ValueMovement::Transfer {
            id: MovementId(format!("move-{id}")),
            asset: AssetId {
                chain: scope().chain,
                asset: "native".to_owned(),
            },
            amount: Decimal::from(1_u64),
            from: address("sender"),
            to: address("receiver"),
        }],
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
            chain: scope().chain,
            asset: "native".to_owned(),
        },
        amount: Decimal::from(1_u64),
        evidence: vec![1, 2, 3],
        created_at: BlockHeight(1),
        coinbase: false,
    }
}

fn commit(
    repository: &TestRepository,
    block: BlockRef,
    checkpoint: Option<BlockRef>,
    drafts: Vec<ObservationDraft>,
    outputs: OutputChanges,
) -> BlockOutcome {
    let addition = BlockAddition::new(
        scope(),
        checkpoint,
        4,
        InterpretedBlock {
            block,
            transactions: drafts,
            outputs,
        },
    )
    .expect("valid block addition");
    block_on(repository.add(addition)).expect("block commit")
}

#[test]
fn commit_stores_only_checkpoint_history_journal_and_live_outputs() {
    let repository = repository();
    let first = block(1, 1, 0);
    let created = output("tx");
    assert_eq!(
        commit(
            &repository,
            first.clone(),
            None,
            vec![draft("tx")],
            OutputChanges {
                created: vec![created.clone()],
                ..OutputChanges::default()
            },
        ),
        BlockOutcome::Applied
    );

    assert_eq!(
        block_on(repository.get(BlockSelector::Tip(scope()))).expect("checkpoint"),
        Some(first.clone())
    );
    assert_eq!(
        block_on(repository.get(BlockSelector::Height {
            scope: scope(),
            height: first.height,
        }))
        .expect("journal block"),
        Some(first)
    );
    let history = block_on(Transactions::list(
        &*repository,
        HistoryQuery {
            scope: scope(),
            address: address("receiver"),
            after: None,
            limit: 10,
        },
    ))
    .expect("history");
    assert_eq!(history.transactions.len(), 1);
    let outputs = block_on(Outputs::list(
        &*repository,
        OutputRequest {
            scope: scope(),
            address: address("receiver"),
            after: None,
            limit: 10,
        },
    ))
    .expect("outputs");
    assert_eq!(outputs.outputs, vec![created]);
}

#[test]
fn revert_removes_canonical_history_and_restores_outputs() {
    let repository = repository();
    let first = block(1, 1, 0);
    let created = output("tx");
    commit(
        &repository,
        first.clone(),
        None,
        vec![draft("tx")],
        OutputChanges {
            created: vec![created],
            ..OutputChanges::default()
        },
    );
    block_on(repository.remove(scope(), first)).expect("revert");

    assert_eq!(
        block_on(repository.get(BlockSelector::Tip(scope()))).expect("checkpoint"),
        None
    );
    assert!(
        block_on(Transactions::list(
            &*repository,
            HistoryQuery {
                scope: scope(),
                address: address("receiver"),
                after: None,
                limit: 10,
            }
        ))
        .expect("history")
        .transactions
        .is_empty()
    );
}

#[test]
fn block_history_preserves_every_address_projection_through_rollback() {
    let repository = repository();
    let first = block(1, 1, 0);
    let mut second = draft("second");
    second.fee = Some(NetworkFee {
        asset: AssetId {
            chain: scope().chain,
            asset: "native".to_owned(),
        },
        amount: Decimal::from(2_u64),
        payer: Some(address("fee-payer")),
    });
    let addition = BlockAddition::new(
        scope(),
        None,
        4,
        InterpretedBlock {
            block: first.clone(),
            transactions: vec![draft("first"), second],
            outputs: OutputChanges::default(),
        },
    )
    .expect("valid multi-address block");
    let expected = addition.transactions().to_vec();
    block_on(repository.add(addition)).expect("block commit");

    for (value, transactions) in [
        ("receiver", expected.clone()),
        ("sender", expected.clone()),
        ("fee-payer", vec![expected[1].clone()]),
    ] {
        let page = block_on(Transactions::list(
            &*repository,
            HistoryQuery {
                scope: scope(),
                address: address(value),
                after: None,
                limit: 10,
            },
        ))
        .expect("canonical address history");
        assert_eq!(page.checkpoint, Some(first.clone()));
        assert_eq!(page.transactions, transactions);
    }

    assert_eq!(
        block_on(repository.remove(scope(), first)).expect("journal rollback"),
        None
    );
    for value in ["receiver", "sender", "fee-payer"] {
        let page = block_on(Transactions::list(
            &*repository,
            HistoryQuery {
                scope: scope(),
                address: address(value),
                after: None,
                limit: 10,
            },
        ))
        .expect("address history after rollback");
        assert_eq!(page.checkpoint, None);
        assert!(page.transactions.is_empty());
    }
}

#[test]
fn reverting_a_spend_restores_the_live_output() {
    let repository = repository();
    let first = block(1, 1, 0);
    let created = output("funding");
    commit(
        &repository,
        first.clone(),
        None,
        vec![draft("funding")],
        OutputChanges {
            created: vec![created.clone()],
            ..OutputChanges::default()
        },
    );
    let second = block(2, 2, 1);
    commit(
        &repository,
        second.clone(),
        Some(first.clone()),
        vec![draft("spend")],
        OutputChanges {
            spent: vec![created.key()],
            ..OutputChanges::default()
        },
    );
    assert!(
        block_on(Outputs::list(
            &*repository,
            OutputRequest {
                scope: scope(),
                address: address("receiver"),
                after: None,
                limit: 10,
            }
        ))
        .expect("spent output page")
        .outputs
        .is_empty()
    );

    block_on(repository.remove(scope(), second)).expect("revert spend");

    let outputs = block_on(Outputs::list(
        &*repository,
        OutputRequest {
            scope: scope(),
            address: address("receiver"),
            after: None,
            limit: 10,
        },
    ))
    .expect("restored output page");
    assert_eq!(outputs.outputs, vec![created]);
    assert_eq!(outputs.checkpoint, Some(first));
}

#[cfg(test)]
impl TestRepository {
    fn snapshot(&self) -> storage::ScanPage {
        use storage::Store;

        block_on(self.storage.scan(storage::ScanRequest {
            namespace: crate::keys::namespace(),
            prefix: Vec::new(),
            after: None,
            limit: 1_000,
        }))
        .unwrap()
    }

    fn record<T: bincode::Decode<()>>(&self, key: &storage::Key) -> T {
        use storage::Store;

        let stored = block_on(self.storage.get(&crate::keys::namespace(), key))
            .unwrap()
            .unwrap();
        bincode::decode_from_slice(&stored.value.0, bincode::config::standard())
            .unwrap()
            .0
    }

    fn overwrite<T: bincode::Encode>(&self, key: storage::Key, record: &T) {
        use storage::Store;

        let value =
            storage::Value(bincode::encode_to_vec(record, bincode::config::standard()).unwrap());
        let operation = storage::Operation::Put {
            namespace: crate::keys::namespace(),
            key,
            value,
        };
        block_on(self.storage.commit(storage::WriteBatch {
            conditions: Vec::new(),
            operations: vec![operation],
        }))
        .unwrap();
    }
}

#[cfg(test)]
#[test]
fn late_required_spend_failure_does_not_publish_queued_history_or_created_outputs() {
    let repository = repository();
    let first = block(1, 1, 0);
    commit(
        &repository,
        first.clone(),
        None,
        vec![draft("first")],
        OutputChanges::default(),
    );
    let mut created = output("created");
    created.created_at = BlockHeight(2);
    let addition = BlockAddition::new(
        scope(),
        Some(first),
        4,
        InterpretedBlock {
            block: block(2, 2, 1),
            transactions: vec![draft("second")],
            outputs: OutputChanges {
                created: vec![created],
                spent: vec![output("missing").key()],
                ..OutputChanges::default()
            },
        },
    )
    .unwrap();
    let before = repository.snapshot();
    let error = block_on(repository.add(addition)).unwrap_err();
    assert_eq!(error.kind, indexing::IndexErrorKind::InvalidBlock);
    assert_eq!(error.message, "block spends an unknown indexed output");
    assert!(!error.retryable);
    assert_eq!(repository.snapshot(), before);
}

#[cfg(test)]
#[test]
fn late_rollback_corruption_or_restore_conflict_leaves_every_stored_byte_unchanged() {
    for corruption in 0..3 {
        let repository = repository();
        let first = block(1, 1, 0);
        let second = block(2, 2, 1);
        let funding = output("funding");
        let created = OutputChanges {
            created: vec![funding.clone()],
            ..OutputChanges::default()
        };
        commit(
            &repository,
            first.clone(),
            None,
            vec![draft("first")],
            created,
        );
        let mut created = output("second");
        created.created_at = second.height;
        let changes = OutputChanges {
            created: vec![created],
            spent: vec![funding.key()],
            ..OutputChanges::default()
        };
        commit(
            &repository,
            second.clone(),
            Some(first),
            vec![draft("second")],
            changes,
        );
        let key = crate::keys::journal(&scope(), second.height);
        let mut journal = repository.record::<crate::record::JournalRecord>(&key);
        let expected = match corruption {
            0 => {
                journal.restore_outputs[0].key = vec![255];
                "rollback journal contains a foreign restored-output key"
            }
            1 => {
                journal.restore_outputs[0].value =
                    crate::record::OutputRecord::from_domain(&output("other"));
                "restored output key does not match its journal value"
            }
            _ => {
                repository.overwrite(
                    crate::keys::output(&scope(), &funding.key()),
                    &crate::record::OutputRecord::from_domain(&funding),
                );
                "output to restore already exists"
            }
        };
        repository.overwrite(key, &journal);
        let before = repository.snapshot();
        let error = block_on(repository.remove(scope(), second)).unwrap_err();
        assert_eq!(error.message, expected);
        assert_eq!(error.retryable, corruption == 2);
        assert_eq!(repository.snapshot(), before);
    }
}

#[cfg(test)]
#[test]
fn duplicate_commit_requires_its_matching_journal_before_other_admission_checks() {
    let repository = repository();
    let first = block(1, 1, 0);
    let addition = || {
        BlockAddition::new(
            scope(),
            None,
            4,
            InterpretedBlock {
                block: first.clone(),
                transactions: vec![draft("first")],
                outputs: OutputChanges::default(),
            },
        )
        .unwrap()
    };
    assert_eq!(
        block_on(repository.add(addition())).unwrap(),
        BlockOutcome::Applied
    );
    assert_eq!(
        block_on(repository.add(addition())).unwrap(),
        BlockOutcome::AlreadyApplied
    );
    let key = crate::keys::journal(&scope(), first.height);
    let mut journal = repository.record::<crate::record::JournalRecord>(&key);
    journal.block = crate::record::BlockRecord::from_domain(&block(1, 2, 0));
    repository.overwrite(key, &journal);
    let before = repository.snapshot();
    let error = block_on(repository.add(addition())).unwrap_err();
    assert_eq!(error.kind, indexing::IndexErrorKind::Store);
    assert_eq!(
        error.message,
        "canonical checkpoint is missing its rollback journal"
    );
    assert!(!error.retryable);
    assert_eq!(repository.snapshot(), before);
}

#[cfg(test)]
#[test]
fn page_validation_preserves_scope_address_limit_then_cursor_precedence() {
    let repository = repository();
    let foreign = IndexScope {
        network: "other".into(),
        ..scope()
    };
    let bad_address = CanonicalAddress {
        scope: foreign.clone(),
        value: "receiver".into(),
    };
    let checkpoint = Some(block(1, 1, 0));
    let cases = [
        (
            foreign,
            bad_address.clone(),
            0,
            "request belongs to another repository scope",
        ),
        (
            scope(),
            bad_address,
            0,
            "address belongs to another repository scope",
        ),
        (
            scope(),
            address("receiver"),
            0,
            "query limit must be between 1 and 1000",
        ),
        (scope(), address("receiver"), 1, ""),
    ];
    for (scope, address, limit, message) in cases {
        let output_error = block_on(Outputs::list(
            &*repository,
            OutputRequest {
                scope: scope.clone(),
                address: address.clone(),
                limit,
                after: Some(indexing::OutputCursor {
                    checkpoint: checkpoint.clone(),
                    position: vec![255],
                }),
            },
        ))
        .unwrap_err();
        let history_cursor = indexing::HistoryCursor {
            checkpoint: checkpoint.clone(),
            position: indexing::HistoryPosition {
                height: BlockHeight(99),
                transaction: transaction("z"),
            },
        };
        let history_error = block_on(Transactions::list(
            &*repository,
            HistoryQuery {
                scope,
                address,
                limit,
                after: Some(history_cursor),
            },
        ))
        .unwrap_err();
        if limit == 1 {
            assert_eq!(output_error.message, "outputs changed during pagination");
            assert_eq!(history_error.message, "history changed during pagination");
            assert!(output_error.retryable && history_error.retryable);
        } else {
            assert_eq!(output_error.message, message);
            assert_eq!(history_error.message, message);
            assert!(!output_error.retryable && !history_error.retryable);
        }
    }
}
