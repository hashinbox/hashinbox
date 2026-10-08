//! A holding's balance as the shipped stores answer it ([`SettledBalance`]),
//! and completed by the blocks an index holds above their line ([`Balance`]).

use num_bigint::BigInt;

use crate::{Address, BlockRef, BlockWithOutputs, Checkpointed, Decimal, Holder, Sample};

/// One holding's balance, in the asset's atomic units.
///
/// `net` is settled: folded over durable rows only, so it cannot revert, and
/// signed, so a deficit (debits whose credits are not indexed) stays visible.
/// The pending figures are what the blocks above the line move, in and out,
/// never netted and never negative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Balance {
    /// Settled signed total.
    pub net: BigInt,
    /// The confirmed line `net` was read at; `None` before the first commit.
    pub observed_at: Option<BlockRef>,
    /// How many blocks the address's inspection is behind `observed_at`
    /// ([`crate::Inspection::behind`]).
    pub behind: u64,
    /// Value arriving above the line.
    pub incoming: Decimal,
    /// Value leaving above the line.
    pub outgoing: Decimal,
    /// The newest block above the line, when one is held.
    pub pending: Option<BlockRef>,
}

/// The settled half of a balance, as one snapshot of a store holds it: the
/// total kept for an observed holding, or its rows folded exactly, with the
/// line it was read at and how far behind that line the address's inspection
/// is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettledBalance {
    net: BigInt,
    observed_at: Option<BlockRef>,
    behind: u64,
}

impl Checkpointed for SettledBalance {
    fn checkpoint(&self) -> Option<&BlockRef> {
        self.observed_at.as_ref()
    }
}

impl SettledBalance {
    /// The answer one snapshot read.
    #[cfg(any(feature = "sqlite", feature = "memory"))]
    pub(crate) const fn new(net: BigInt, observed_at: Option<BlockRef>, behind: u64) -> Self {
        Self {
            net,
            observed_at,
            behind,
        }
    }

    /// The balance with what `sample`'s blocks above this read's line move.
    ///
    /// Sample the pending blocks *before* the store read, so a commit landing
    /// in between is counted once. A movement between two of the holder's own
    /// endpoints is reported as both arriving and leaving: above the line the
    /// question is what is moving, not what the total will be. A block credit
    /// counts as arriving.
    #[must_use]
    pub fn completed(self, of: &dyn Holder, sample: Sample) -> Balance {
        let blocks: Vec<BlockWithOutputs> = sample.above(&self);
        let movements = blocks
            .iter()
            .flat_map(|facts| facts.block.transactions())
            .flat_map(|transaction| &transaction.movements)
            .filter(|movement| movement.asset == *of.asset());
        let (mut incoming, mut outgoing) = (Vec::new(), Vec::new());
        for movement in movements {
            let at = |endpoint: Option<&Address>| endpoint == Some(of.address());
            incoming.extend(at(movement.to()).then(|| movement.amount.clone()));
            outgoing.extend(at(movement.from()).then(|| movement.amount.clone()));
        }
        let credits = blocks
            .iter()
            .flat_map(|facts| facts.credits.as_slice())
            .filter(|credit| credit.asset == *of.asset() && credit.to == *of.address());
        incoming.extend(credits.map(|credit| credit.amount.clone()));
        Balance {
            net: self.net,
            observed_at: self.observed_at,
            behind: self.behind,
            incoming: Decimal::total(&incoming),
            outgoing: Decimal::total(&outgoing),
            pending: blocks.last().map(|held| held.block.reference().clone()),
        }
    }
}
