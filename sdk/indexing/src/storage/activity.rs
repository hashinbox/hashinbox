//! Address-local effects, including credits with no transaction.

use crate::{
    Address, AssetId, BlockHeight, BlockPosition, BlockRef, Completeness, Decimal, Holder,
    IndexError, MovementId, TransactionRef,
};

/// Which side of a movement affected the holding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Direction {
    /// Value arrived.
    Credit,
    /// Value left.
    Debit,
}

/// Why a holding changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityKind {
    /// An ordinary chain-native movement.
    Movement(base::MovementKind),
    /// A separately charged network fee, including fees of failed transactions.
    Fee,
    /// A block credit with no transaction, such as a consensus withdrawal.
    Withdrawal,
}

/// The transaction's native place, independent of address selection.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ActivityTransaction {
    /// Native transaction order in the block.
    pub order: u64,
    /// Canonical transaction identity.
    pub id: TransactionRef,
}

/// An ascending effect cursor. Transactionless credits precede transactions in
/// each block; their identities order them within that separate namespace.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ActivityPosition {
    /// Produced block height.
    pub height: BlockHeight,
    /// Absent for block credits.
    pub transaction: Option<ActivityTransaction>,
    /// Native movement order; separately charged fees sort after movements.
    pub movement_order: u64,
    /// Movement identity; absent for a separately charged fee.
    pub movement: Option<MovementId>,
    /// Final tie-break: both sides of self-transfers remain pageable.
    pub direction: Direction,
}

/// One exact atomic credit or debit of the queried holding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activity {
    /// Stable ordering and resume identity.
    pub position: ActivityPosition,
    /// Native position of the containing block.
    pub block_position: BlockPosition,
    /// The cause of this effect.
    pub kind: ActivityKind,
    /// Nonnegative amount in atomic units.
    pub amount: Decimal,
    /// The opposite endpoint when the movement names one.
    pub counterparty: Option<Address>,
}

/// A bounded page of confirmed effects for one holding.
#[derive(Clone, Debug)]
pub struct ActivityQuery {
    /// Address whose effects are requested.
    pub address: Address,
    /// Asset whose effects are requested.
    pub asset: AssetId,
    /// Resume strictly after this effect.
    pub after: Option<ActivityPosition>,
    /// Number of effects, 1..=1000.
    pub limit: usize,
}

impl ActivityQuery {
    /// The first page for a holding. Validation occurs when the repository reads it.
    #[must_use]
    pub fn new(subject: &dyn Holder, limit: usize) -> Self {
        Self {
            address: subject.address().clone(),
            asset: subject.asset().clone(),
            after: None,
            limit,
        }
    }

    /// Checks the holding, page length and cursor network.
    ///
    /// # Errors
    /// Invalid request when the holding or page is invalid, or its cursor is foreign.
    pub fn validate(&self) -> Result<(), IndexError> {
        crate::holder::require_holding(&self.address, &self.asset)?;
        if self
            .after
            .as_ref()
            .and_then(|position| position.transaction.as_ref())
            .is_some_and(|transaction| transaction.id.network != self.address.network)
        {
            return Err(IndexError::new(
                crate::IndexErrorKind::InvalidRequest,
                "activity cursor belongs to another network",
                false,
            ));
        }
        super::history::require_limit(self.limit)
    }
}

impl Holder for ActivityQuery {
    fn address(&self) -> &Address {
        &self.address
    }
    fn asset(&self) -> &AssetId {
        &self.asset
    }
}

/// Effects and coverage read from one repository snapshot. While backfill is
/// incomplete, a later page can gain earlier rows; restart paging after it settles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityPage {
    /// Confirmed effects in ascending position order.
    pub effects: Vec<Activity>,
    /// Last returned position when more effects exist.
    pub next: Option<ActivityPosition>,
    /// Repository checkpoint of this same snapshot.
    pub observed_at: Option<BlockRef>,
    /// What the snapshot's watch progress proves about completeness.
    pub completeness: Completeness,
}
