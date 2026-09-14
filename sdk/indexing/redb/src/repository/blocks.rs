use super::*;
use ::storage::Store;

impl Blocks for Repository {
    fn get<'a>(
        &'a self,
        selector: BlockSelector,
    ) -> indexing::BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move { self.read_block(selector).await })
    }

    fn add<'a>(
        &'a self,
        addition: BlockAddition,
    ) -> indexing::BoxFuture<'a, Result<BlockOutcome, IndexError>> {
        Box::pin(async move { self.write_block(addition).await })
    }

    fn remove<'a>(
        &'a self,
        scope: IndexScope,
        expected_tip: BlockRef,
    ) -> indexing::BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move { self.remove_tip(&scope, &expected_tip).await })
    }
}

impl Repository {
    async fn read_block(&self, selector: BlockSelector) -> Result<Option<BlockRef>, IndexError> {
        let (scope, height) = match selector {
            BlockSelector::Tip(scope) => (scope, None),
            BlockSelector::Height { scope, height } => (scope, Some(height)),
        };
        self.check_scope(&scope)?;
        Ok(match height {
            Some(height) => self
                .get::<record::JournalRecord>(&keys::journal(&scope, height))
                .await?
                .map(|stored| stored.value.block()),
            None => self
                .get::<record::BlockRecord>(&keys::checkpoint(&scope))
                .await?
                .map(|stored| stored.value.into_domain()),
        })
    }

    async fn write_block(&self, addition: BlockAddition) -> Result<BlockOutcome, IndexError> {
        self.check_scope(addition.scope())?;
        let checkpoint_key = keys::checkpoint(&self.scope);
        let checkpoint = self.get::<record::BlockRecord>(&checkpoint_key).await?;
        let current = checkpoint
            .as_ref()
            .map(|stored| stored.value.clone().into_domain());
        let journal_key = keys::journal(&self.scope, addition.block().height);
        let journal = self.get::<record::JournalRecord>(&journal_key).await?;

        if current.as_ref() == Some(addition.block()) {
            return match journal {
                Some(stored) if stored.value.block() == *addition.block() => {
                    Ok(BlockOutcome::AlreadyApplied)
                }
                _ => Err(Self::record_error(
                    "canonical checkpoint is missing its rollback journal",
                )),
            };
        }
        if journal.is_some() {
            return Err(IndexError::new(
                IndexErrorKind::CannotConnect,
                "another retained block exists at this height",
                true,
            ));
        }
        if current.as_ref() != addition.expected_checkpoint() {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "checkpoint changed before block commit",
                true,
            ));
        }

        self.commit_addition(&addition, checkpoint.as_ref()).await
    }

    async fn commit_addition(
        &self,
        addition: &BlockAddition,
        checkpoint: Option<&VersionedRecord<record::BlockRecord>>,
    ) -> Result<BlockOutcome, IndexError> {
        let checkpoint_key = keys::checkpoint(&self.scope);
        let journal_key = keys::journal(&self.scope, addition.block().height);
        let mut batch = WriteBatch::default();
        Self::expect(&mut batch, checkpoint_key.clone(), checkpoint);
        Self::expect::<record::JournalRecord>(&mut batch, journal_key.clone(), None);
        let journal = record::JournalRecord::for_addition(self, addition, &mut batch).await?;
        Self::put(&mut batch, journal_key, &journal)?;
        Self::put(
            &mut batch,
            checkpoint_key,
            &record::BlockRecord::from_domain(addition.block()),
        )?;
        if addition.block().height.0 >= addition.retention() {
            let height = addition.block().height.0 - addition.retention();
            Self::delete(
                &mut batch,
                keys::journal(&self.scope, indexing::BlockHeight(height)),
            );
        }

        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(BlockOutcome::Applied)
    }

    async fn write_history(
        &self,
        transactions: &[indexing::CanonicalTransaction],
        batch: &mut WriteBatch,
    ) -> Result<Vec<Vec<u8>>, IndexError> {
        let mut history_keys = Vec::new();
        let history = transactions.iter().flat_map(|transaction| {
            transaction
                .addresses()
                .into_iter()
                .map(move |address| (transaction, address))
        });
        for (transaction, address) in history {
            let key = keys::history(
                &self.scope,
                &address,
                transaction.block().height,
                &transaction.transaction_id,
            );
            let existing = self.get::<record::TransactionRecord>(&key).await?;
            if existing.is_some() {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "canonical history entry already exists",
                    true,
                ));
            }
            Self::expect::<record::TransactionRecord>(batch, key.clone(), None);
            Self::put(
                batch,
                key.clone(),
                &record::TransactionRecord::from_domain(transaction),
            )?;
            history_keys.push(key.0);
        }

        Ok(history_keys)
    }

    async fn create_outputs(
        &self,
        outputs: &[indexing::IndexedOutput],
        batch: &mut WriteBatch,
    ) -> Result<Vec<Vec<u8>>, IndexError> {
        let mut remove_output_keys = Vec::new();
        for output in outputs {
            let key = keys::output(&self.scope, &output.key());
            let existing = self.get::<record::OutputRecord>(&key).await?;
            if existing.is_some() {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "created output already exists",
                    true,
                ));
            }
            Self::expect::<record::OutputRecord>(batch, key.clone(), None);
            Self::put(
                batch,
                key.clone(),
                &record::OutputRecord::from_domain(output),
            )?;
            remove_output_keys.push(key.0);
        }

        Ok(remove_output_keys)
    }

    async fn spend_outputs(
        &self,
        outputs: &indexing::OutputChanges,
        batch: &mut WriteBatch,
    ) -> Result<Vec<record::RestoredOutput>, IndexError> {
        let required = outputs.spent.iter().map(|key| (key, true));
        let tracked = outputs.tracked_spends.iter().map(|key| (key, false));
        let mut restore_outputs = Vec::new();
        for (output, required) in required.chain(tracked) {
            let key = keys::output(&self.scope, output);
            let existing = self.get::<record::OutputRecord>(&key).await?;
            let existing = match existing {
                Some(existing) => existing,
                None if required => {
                    return Err(IndexError::new(
                        IndexErrorKind::InvalidBlock,
                        "block spends an unknown indexed output",
                        false,
                    ));
                }
                None => continue,
            };
            Self::expect(batch, key.clone(), Some(&existing));
            Self::delete(batch, key.clone());
            restore_outputs.push(record::RestoredOutput {
                key: key.0,
                value: existing.value,
            });
        }

        Ok(restore_outputs)
    }

    async fn remove_tip(
        &self,
        scope: &IndexScope,
        expected_tip: &BlockRef,
    ) -> Result<Option<BlockRef>, IndexError> {
        self.check_scope(scope)?;
        let checkpoint_key = keys::checkpoint(scope);
        let checkpoint = self.get::<record::BlockRecord>(&checkpoint_key).await?;
        if checkpoint
            .as_ref()
            .map(|stored| stored.value.clone().into_domain())
            .as_ref()
            != Some(expected_tip)
        {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "revert must target the current checkpoint",
                true,
            ));
        }
        let journal_key = keys::journal(scope, expected_tip.height);
        let journal = self
            .get::<record::JournalRecord>(&journal_key)
            .await?
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::ReorgTooDeep,
                    "rollback journal is not retained",
                    false,
                )
            })?;
        if journal.value.block() != *expected_tip {
            return Err(Self::record_error(
                "rollback journal does not match the canonical tip",
            ));
        }

        self.revert_journal(checkpoint_key, checkpoint.as_ref(), journal_key, &journal)
            .await
    }

    async fn revert_journal(
        &self,
        checkpoint_key: Key,
        checkpoint: Option<&VersionedRecord<record::BlockRecord>>,
        journal_key: Key,
        journal: &VersionedRecord<record::JournalRecord>,
    ) -> Result<Option<BlockRef>, IndexError> {
        let mut batch = WriteBatch::default();
        Self::expect(&mut batch, checkpoint_key.clone(), checkpoint);
        Self::expect(&mut batch, journal_key.clone(), Some(journal));
        self.remove_history(&journal.value.history_keys, &mut batch)
            .await?;
        self.remove_outputs(&journal.value.remove_output_keys, &mut batch)
            .await?;
        self.restore_outputs(&journal.value.restore_outputs, &mut batch)
            .await?;

        Self::delete(&mut batch, journal_key);
        let restored = journal
            .value
            .previous_checkpoint
            .clone()
            .map(record::BlockRecord::into_domain);
        match &journal.value.previous_checkpoint {
            Some(value) => Self::put(&mut batch, checkpoint_key, value)?,
            None => Self::delete(&mut batch, checkpoint_key),
        }
        self.storage
            .commit(batch)
            .await
            .map_err(Self::storage_error)?;
        Ok(restored)
    }

    async fn remove_history(
        &self,
        history_keys: &[Vec<u8>],
        batch: &mut WriteBatch,
    ) -> Result<(), IndexError> {
        for key in history_keys {
            if !keys::is_history(&self.scope, key) {
                return Err(Self::record_error(
                    "rollback journal contains a foreign history key",
                ));
            }
            let key = Key(key.clone());
            let existing = self
                .get::<record::TransactionRecord>(&key)
                .await?
                .ok_or_else(|| Self::record_error("rollback history entry is missing"))?;
            existing.value.clone().into_domain()?;
            Self::expect(batch, key.clone(), Some(&existing));
            Self::delete(batch, key);
        }
        Ok(())
    }

    async fn remove_outputs(
        &self,
        output_keys: &[Vec<u8>],
        batch: &mut WriteBatch,
    ) -> Result<(), IndexError> {
        for key in output_keys {
            if !keys::is_output(&self.scope, key) {
                return Err(Self::record_error(
                    "rollback journal contains a foreign output key",
                ));
            }
            let key = Key(key.clone());
            let existing = self
                .get::<record::OutputRecord>(&key)
                .await?
                .ok_or_else(|| Self::record_error("rollback output is missing"))?;
            let output = existing.value.clone().into_domain()?;
            if keys::output(&self.scope, &output.key()) != key {
                return Err(Self::record_error(
                    "rollback output key does not match its stored value",
                ));
            }
            Self::expect(batch, key.clone(), Some(&existing));
            Self::delete(batch, key);
        }
        Ok(())
    }

    async fn restore_outputs(
        &self,
        outputs: &[record::RestoredOutput],
        batch: &mut WriteBatch,
    ) -> Result<(), IndexError> {
        for restored in outputs {
            if !keys::is_output(&self.scope, &restored.key) {
                return Err(Self::record_error(
                    "rollback journal contains a foreign restored-output key",
                ));
            }
            let key = Key(restored.key.clone());
            let output = restored.value.clone().into_domain()?;
            if keys::output(&self.scope, &output.key()) != key {
                return Err(Self::record_error(
                    "restored output key does not match its journal value",
                ));
            }
            let existing = self.get::<record::OutputRecord>(&key).await?;
            if existing.is_some() {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "output to restore already exists",
                    true,
                ));
            }
            Self::expect::<record::OutputRecord>(batch, key.clone(), None);
            Self::put(batch, key, &restored.value)?;
        }

        Ok(())
    }
}

impl record::JournalRecord {
    async fn for_addition(
        repository: &Repository,
        addition: &BlockAddition,
        batch: &mut WriteBatch,
    ) -> Result<Self, IndexError> {
        let history_keys = repository
            .write_history(addition.transactions(), batch)
            .await?;
        let remove_output_keys = repository
            .create_outputs(&addition.outputs().created, batch)
            .await?;
        let restore_outputs = repository.spend_outputs(addition.outputs(), batch).await?;
        Ok(Self {
            block: record::BlockRecord::from_domain(addition.block()),
            previous_checkpoint: addition
                .expected_checkpoint()
                .map(record::BlockRecord::from_domain),
            history_keys,
            remove_output_keys,
            restore_outputs,
        })
    }
}
