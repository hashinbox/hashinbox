//! Immutable attestation capture, stored beside the watched balance.

use super::{Repository, Tables};
use crate::{BalanceEvidence, BalanceQuery, BlockRef, Holder, IndexError};

impl Repository {
    /// Records externally agreed evidence only while its snapshot still matches.
    /// Never changes the indexed balance or history completeness.
    ///
    /// # Errors
    /// Retryable conflict for stale/unwatched/incomplete evidence; lock failure.
    pub async fn record_evidence(&self, evidence: BalanceEvidence) -> Result<(), IndexError> {
        self.apply(|tables| {
            let key = (
                evidence.address().network.clone(),
                evidence.asset().clone(),
                evidence.address().value.clone(),
            );
            let observations = Self::observing(tables, evidence.address());
            evidence.admits(
                tables.checkpoints.get(&key.0),
                tables.balances.get(&key),
                &observations,
            )?;
            tables.openings.insert(key, evidence);
            Ok(())
        })
    }

    pub(super) fn opening(
        query: &BalanceQuery,
        tables: &Tables,
        checkpoint: Option<&BlockRef>,
    ) -> Option<BalanceEvidence> {
        let key = (
            query.address.network.clone(),
            query.asset.clone(),
            query.address.value.clone(),
        );
        let evidence = tables.openings.get(&key)?;
        evidence
            .visible(checkpoint, &Self::observing(tables, &query.address))
            .then(|| evidence.clone())
    }
}

impl Tables {
    /// A transient insertion order cannot invalidate an otherwise sound block.
    pub(super) fn clear_deficits(&mut self, network: &crate::Network) {
        self.openings.retain(|key, _| {
            key.0 != *network
                || self
                    .balances
                    .get(key)
                    .is_some_and(|net| net.sign() != num_bigint::Sign::Minus)
        });
    }
}
