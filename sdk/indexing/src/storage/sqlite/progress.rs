//! What a commit records onto an observation's own row: how far it has been
//! inspected.
//!
//! It lives on `watch` rather than in a table of its own, so withdrawing an
//! observation takes its claim with it by removing one row, and a claim can
//! never be paired with the wrong observation because there is no second row to
//! pair.
//!
//! The progress write is a compare-and-set against the value the claim's author
//! says it read. It is deliberately **not** a monotonic rule: a batch redone
//! under a smaller budget legitimately settles less than the write it is
//! redoing, so what this refuses is a claim whose author was reading a different
//! value.
//!

use rusqlite::{Connection, Transaction, params};

use super::{Tables, conflict, failure};
use crate::{BlockRef, IndexError, Progress};

impl Tables {
    /// The progress recorded for one observation of this network, or `None` when
    /// nothing has been.
    pub(super) fn recorded(
        &self,
        connection: &Connection,
        watch: &str,
    ) -> Result<Option<BlockRef>, IndexError> {
        let held = connection
            .query_row(
                "SELECT indexed_position, indexed_height, indexed_hash,
                        indexed_parent_position, indexed_parent_hash, indexed_timestamp
                 FROM watch WHERE chain = ?1 AND network = ?2 AND id = ?3",
                params![self.chain(), self.name(), watch],
                |row| {
                    row.get::<_, Option<i64>>(0)?
                        .is_some()
                        .then(|| Self::coordinates(row, 0))
                        .transpose()
                },
            )
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                error => Err(failure(&error, "failed to read recorded progress")),
            })?;
        held.map(Self::block).transpose()
    }

    /// Records one claim, after comparing what it expected against what is held.
    ///
    /// Two refusals, both retryable, and both about the caller's belief rather
    /// than the store's state: a claim naming an observation this file does not
    /// hold, and a claim whose `expected` is not what the row says.
    pub(super) fn advance(
        &self,
        transaction: &Transaction<'_>,
        claim: &Progress,
    ) -> Result<(), IndexError> {
        if !self.registered(transaction, &claim.watch_id)? {
            return Err(conflict(
                "indexed progress names an observation this store does not hold",
            ));
        }
        if self.recorded(transaction, &claim.watch_id)?.as_ref() != claim.expected.as_ref() {
            return Err(conflict("recorded progress changed before this commit"));
        }
        let (position, height, hash, parent_position, parent_hash, timestamp) =
            Self::columns(&claim.indexed_through)?;
        transaction
            .prepare_cached(
                "UPDATE watch SET indexed_position = ?4, indexed_height = ?5,
                     indexed_hash = ?6, indexed_parent_position = ?7,
                     indexed_parent_hash = ?8, indexed_timestamp = ?9
                 WHERE chain = ?1 AND network = ?2 AND id = ?3",
            )
            .and_then(|mut statement| {
                statement.execute(params![
                    self.chain(),
                    self.name(),
                    claim.watch_id,
                    position,
                    height,
                    hash,
                    parent_position,
                    parent_hash,
                    timestamp,
                ])
            })
            .map(drop)
            .map_err(|error| failure(&error, "failed to record indexed progress"))
    }

    /// Resets everything every observation of this network claimed, because the
    /// rows behind all of it have just been removed.
    pub(super) fn disclaim(&self, transaction: &Transaction<'_>) -> Result<(), IndexError> {
        transaction
            .execute(
                "UPDATE watch SET balance = '0',
                     indexed_position = NULL, indexed_height = NULL, indexed_hash = NULL,
                     indexed_parent_position = NULL, indexed_parent_hash = NULL,
                     indexed_timestamp = NULL,
                     evidence_position = NULL, evidence_height = NULL, evidence_hash = NULL,
                     evidence_parent_position = NULL, evidence_parent_hash = NULL,
                     evidence_timestamp = NULL, evidence_amount = NULL
                 WHERE chain = ?1 AND network = ?2",
                params![self.chain(), self.name()],
            )
            .map(drop)
            .map_err(|error| failure(&error, "failed to reset a purged network's claims"))
    }

    /// Whether this network holds an observation under `watch`.
    fn registered(&self, transaction: &Transaction<'_>, watch: &str) -> Result<bool, IndexError> {
        transaction
            .query_row(
                "SELECT count(*) FROM watch
                 WHERE chain = ?1 AND network = ?2 AND id = ?3",
                params![self.chain(), self.name(), watch],
                |row| row.get::<_, i64>(0),
            )
            .map(|held| held > 0)
            .map_err(|error| failure(&error, "failed to read an observation"))
    }
}

/// What the shared write contract below the line cannot be asked, because only a
/// store that keeps movements as their own rows has them: the `balance_movement`
/// rows a refused batch must also roll back.
///
/// The contract itself runs in `tests/repository_contract.rs`, against this
/// store and the in-memory one together, which is the only definition this
/// repository has of two stores being the same store.
#[cfg(test)]
mod progress_test {
    use base::Decimal;
    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::super::{
        Repository,
        database_test::{block_on, network, open},
    };
    use super::*;
    use crate::{
        Address, AssetId, BlockHash, BlockHeight, BlockParent, BlockPosition, CanonicalBlock,
        FeeSettlement, IndexErrorKind, IndexStore, IndexedBatch, InterpretedBlock, MovementId,
        NetworkFee, ObservationDraft, ObservationDraftStatus, TransactionRef, ValueMovement, Watch,
        WatchStore,
    };

    fn block(height: u64) -> BlockRef {
        let hash = |height: u64| BlockHash(height.to_be_bytes().to_vec());
        BlockRef {
            position: BlockPosition(height),
            height: BlockHeight(height),
            hash: hash(height),
            parent: Some(BlockParent {
                position: BlockPosition(height - 1),
                hash: hash(height - 1),
            }),
            timestamp: Some(1_000 + height),
        }
    }

    fn address(value: &str) -> Address {
        Address::new(network(), value)
    }

    /// One block as the interpreter reports it for the address that was not
    /// watched when it was committed; `amount` is what makes a re-presented
    /// block disagree with the stored one.
    fn canonical(height: u64, amount: u64) -> CanonicalBlock {
        let asset = AssetId::native(network().chain);
        CanonicalBlock::new(
            network(),
            InterpretedBlock::new(
                &network(),
                block(height),
                vec![ObservationDraft {
                    transaction_id: TransactionRef::new(network(), format!("block-{height}")),
                    transaction_order: 0,
                    status: ObservationDraftStatus::Included,
                    movements: vec![ValueMovement::transfer(
                        MovementId(format!("m-{height}")),
                        asset.clone(),
                        Decimal::from(amount),
                        address("payer"),
                        address("late"),
                    )],
                    fee: Some(NetworkFee {
                        asset,
                        amount: Decimal::from(1_u64),
                        payer: Some(address("payer")),
                    }),
                }],
            )
            .expect("unique transaction ids"),
        )
        .expect("valid fixture facts")
    }

    /// How many rows one table holds.
    fn rows(repository: &Repository, table: &'static str) -> u64 {
        let counted = move |connection: &mut Connection| {
            let statement = format!("SELECT count(*) FROM {table}");
            connection
                .query_row(&statement, [], |row| row.get::<_, i64>(0))
                .map(|count| count as u64)
                .map_err(|error| failure(&error, "count"))
        };
        block_on(repository.database.blocking(counted)).expect("row count")
    }

    /// **Atomicity, in the rows only this backend keeps separately.** The shared
    /// contract asserts a refused batch writes no history row and records no
    /// claim; this asserts the `movement` rows behind those history rows go with
    /// them, which a store that nests movements inside its history entry cannot
    /// be asked.
    #[test]
    fn a_refused_batch_rolls_back_its_movement_rows_too() {
        let directory = TempDir::new().expect("temporary directory");
        let repository = open(&directory, "atomic.db");
        let late = Watch {
            id: "late".to_owned(),
            address: address("late"),
            asset: AssetId::native(network().chain),
            start: BlockPosition(1),
        };
        block_on(repository.add(vec![late.clone()])).expect("the observation");
        let mut checkpoint = None;
        for height in 1..=5 {
            let empty = CanonicalBlock::new(
                network(),
                InterpretedBlock::new(&network(), block(height), Vec::new())
                    .expect("unique transaction ids"),
            )
            .expect("a valid fixture block");
            let batch = IndexedBatch::new(
                network(),
                checkpoint,
                vec![empty],
                Vec::new(),
                FeeSettlement::Charged,
            )
            .expect("valid");
            block_on(repository.commit(batch)).expect("fixture block");
            checkpoint = Some(block(height));
        }
        let first = IndexedBatch::new(
            network(),
            Some(block(5)),
            vec![canonical(5, 10)],
            vec![Progress {
                watch_id: late.id.clone(),
                expected: None,
                indexed_through: block(5),
            }],
            FeeSettlement::Charged,
        )
        .expect("a valid batch");
        block_on(repository.commit(first)).expect("the first batch");
        let before = [
            rows(&repository, "transaction_address"),
            rows(&repository, "balance_movement"),
        ];

        // Blocks 3 and 4 are new rows; block 5 is already stored and this batch
        // presents it with a different amount.
        let disagreeing = IndexedBatch::new(
            network(),
            Some(block(5)),
            vec![canonical(3, 10), canonical(4, 10), canonical(5, 99)],
            Vec::new(),
            FeeSettlement::Charged,
        )
        .expect("a valid batch");
        let error = block_on(repository.commit(disagreeing)).expect_err("a disagreeing row");

        assert_eq!(error.kind, IndexErrorKind::Store);
        assert!(!error.retryable, "retrying cannot repair the store");
        assert_eq!(
            [
                rows(&repository, "transaction_address"),
                rows(&repository, "balance_movement")
            ],
            before,
            "the rows of blocks 3 and 4 were written before the refusal and must be gone",
        );
        assert_eq!(
            block_on(repository.all(&network()))
                .expect("observations")
                .into_iter()
                .filter_map(|observed| observed.indexed_through)
                .collect::<Vec<_>>(),
            [block(5)],
            "and the claim the first batch proved is untouched",
        );
    }
}
