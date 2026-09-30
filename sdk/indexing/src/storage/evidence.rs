//! An immutable balance observation captured before external verification.

use crate::{
    Address, AssetId, Balance, BalanceQuery, BlockRef, Completeness, Decimal, Holder, IndexError,
    IndexErrorKind,
};

/// One holding's exact settled balance at one block. An attester verifies this
/// same value, then the caller records it; neither step may recapture newer facts.
/// Recording evidence never changes the indexed amount or claims complete history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BalanceEvidence {
    holding: BalanceQuery,
    at: BlockRef,
    amount: Decimal,
}

impl BalanceEvidence {
    /// Captures a settled answer for external verification. The caller must pass
    /// the query that produced `balance`, and record only an agreed attestation.
    ///
    /// # Errors
    /// Invalid holding, absent checkpoint, overdrawn balance or non-atomic amount.
    pub fn capture(query: &BalanceQuery, balance: &Balance) -> Result<Self, IndexError> {
        if balance.completeness == Completeness::Overdrawn {
            return Err(Self::invalid(
                "an overdrawn balance cannot be opening evidence",
            ));
        }
        let at = balance
            .observed_at
            .clone()
            .ok_or_else(|| Self::invalid("balance evidence requires a checkpoint"))?;
        Self::restored(query.clone(), at, balance.amount.clone())
    }

    /// The exact block whose state the attester must verify.
    #[must_use]
    pub const fn at(&self) -> &BlockRef {
        &self.at
    }

    /// The exact nonnegative atomic amount the attester must verify.
    #[must_use]
    pub const fn amount(&self) -> &Decimal {
        &self.amount
    }

    pub(crate) fn restored(
        holding: BalanceQuery,
        at: BlockRef,
        amount: Decimal,
    ) -> Result<Self, IndexError> {
        holding.validate()?;
        amount
            .to_atomic(0)
            .map_err(|_| Self::invalid("balance evidence must be atomic"))?;
        Ok(Self {
            holding,
            at,
            amount,
        })
    }

    fn invalid(message: &'static str) -> IndexError {
        IndexError::new(IndexErrorKind::InvalidRequest, message, false)
    }
}

impl Holder for BalanceEvidence {
    fn address(&self) -> &Address {
        &self.holding.address
    }
    fn asset(&self) -> &AssetId {
        &self.holding.asset
    }
}

#[cfg(any(feature = "sqlite", feature = "memory"))]
impl BalanceEvidence {
    pub(crate) fn admits(
        &self,
        checkpoint: Option<&BlockRef>,
        net: Option<&num_bigint::BigInt>,
        observations: &[crate::WatchProgress],
    ) -> Result<(), IndexError> {
        let units = self
            .amount
            .to_atomic(0)
            .map_err(|_| Self::invalid("balance evidence must be atomic"))?;
        let signed = num_bigint::BigInt::from(units);
        let watched = observations
            .iter()
            .any(|row| row.watch.address == *self.address() && row.watch.asset == *self.asset());
        if checkpoint != Some(&self.at)
            || net != Some(&signed)
            || !watched
            || !self.visible(checkpoint, observations)
        {
            return Err(IndexError::retryable_conflict(
                "balance evidence no longer matches a caught-up watched holding",
            ));
        }
        Ok(())
    }

    pub(crate) fn visible(
        &self,
        checkpoint: Option<&BlockRef>,
        observations: &[crate::WatchProgress],
    ) -> bool {
        checkpoint.is_some_and(|line| line.position > self.at.position || line == &self.at)
            && observations.iter().all(|row| row.caught_up(checkpoint))
    }
}
