//! Checkpoint-bound attestations on existing watched holdings.

use super::{Repository, Tables, failure};
use crate::{BalanceEvidence, BalanceQuery, BlockRef, Holder, IndexError};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

impl Repository {
    /// Records externally agreed evidence if its captured holding, checkpoint and
    /// exact signed amount still match one caught-up snapshot. Never changes money.
    ///
    /// # Errors
    /// Retryable conflict for stale/unwatched/incomplete evidence; storage errors.
    pub async fn record_evidence(&self, evidence: BalanceEvidence) -> Result<(), IndexError> {
        let tables = self.tables(&evidence.address().network)?;
        self.database
            .transaction(move |read| tables.record_evidence(read, &evidence))
            .await
    }
}

impl Tables {
    fn record_evidence(
        &self,
        read: &Transaction<'_>,
        evidence: &BalanceEvidence,
    ) -> Result<(), IndexError> {
        let checkpoint = self.checkpoint(read)?;
        let subject = (
            evidence.address().value.as_str(),
            evidence.asset().asset.as_str(),
        );
        let net = self.saved(read, subject)?;
        let observations = self.observing(read, subject.0)?;
        evidence.admits(checkpoint.as_ref(), net.as_ref(), &observations)?;
        let (position, height, hash, parent_position, parent_hash, timestamp) =
            Self::columns(evidence.at())?;
        read.execute(
            "UPDATE watch SET evidence_position = ?5, evidence_height = ?6, evidence_hash = ?7,
                evidence_parent_position = ?8, evidence_parent_hash = ?9,
                evidence_timestamp = ?10, evidence_amount = ?11
             WHERE chain = ?1 AND network = ?2 AND address = ?3 AND asset = ?4",
            params![
                self.chain(),
                self.name(),
                subject.0,
                subject.1,
                position,
                height,
                hash,
                parent_position,
                parent_hash,
                timestamp,
                evidence.amount().to_string()
            ],
        )
        .map(drop)
        .map_err(|error| failure(&error, "failed to record balance evidence"))
    }

    pub(super) fn opening(
        &self,
        read: &Connection,
        query: &BalanceQuery,
        checkpoint: Option<&BlockRef>,
    ) -> Result<Option<BalanceEvidence>, IndexError> {
        let row = read.query_row(
            "SELECT evidence_position, evidence_height, evidence_hash, evidence_parent_position,
                    evidence_parent_hash, evidence_timestamp, evidence_amount
             FROM watch WHERE chain = ?1 AND network = ?2 AND address = ?3 AND asset = ?4
                AND evidence_position IS NOT NULL",
            params![self.chain(), self.name(), query.address.value, query.asset.asset],
            |row| Ok((Self::coordinates(row, 0)?, row.get::<_, String>(6)?)),
        ).optional().map_err(|error| failure(&error, "failed to read balance evidence"))?;
        let Some((coordinates, amount)) = row else {
            return Ok(None);
        };
        let evidence = BalanceEvidence::restored(
            query.clone(),
            Self::block(coordinates)?,
            Self::amount(&amount)?,
        )?;
        let observations = self.observing(read, &query.address.value)?;
        Ok(evidence
            .visible(checkpoint, &observations)
            .then_some(evidence))
    }

    pub(super) fn invalidate_evidence(
        &self,
        read: &Transaction<'_>,
        subject: (&str, &str),
        height: i64,
    ) -> Result<(), IndexError> {
        read.execute(
            "UPDATE watch SET evidence_position = NULL, evidence_height = NULL,
                evidence_hash = NULL, evidence_parent_position = NULL, evidence_parent_hash = NULL,
                evidence_timestamp = NULL, evidence_amount = NULL
             WHERE chain = ?1 AND network = ?2 AND address = ?3 AND asset = ?4
               AND evidence_height >= ?5",
            params![self.chain(), self.name(), subject.0, subject.1, height],
        )
        .map(drop)
        .map_err(|error| failure(&error, "failed to invalidate balance evidence"))
    }
}
