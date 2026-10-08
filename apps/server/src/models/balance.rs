//! A holding's balance on both sides of its chain's confirmed line, read from
//! indexing's rows (`wallet`, `ledger`, `network_checkpoint`) and completed by
//! the blocks its index holds above that line.

use base::{Address, BlockRef, Decimal, Holder};
use indexing::{BlockWithOutputs, Checkpointed, IndexError, IndexErrorKind, Sample};
use num_bigint::{BigInt, BigUint};
use sqlx::{Row as _, SqliteConnection};

use super::{Checkpoint, Observation};

/// One holding's balance, in the asset's atomic units.
///
/// `net` is settled: folded over durable rows only, so it cannot revert, and
/// signed, so a deficit (debits whose credits are not indexed) stays visible.
/// The pending figures are what the blocks above the line move, in and out,
/// never netted and never negative.
pub(crate) struct Balance {
    /// Settled signed total.
    pub(crate) net: BigInt,
    /// The confirmed line `net` was read at; `None` before the first commit.
    pub(crate) observed_at: Option<BlockRef>,
    /// How many blocks the address's inspection is behind `observed_at`.
    pub(crate) behind: u64,
    /// Value arriving above the line.
    pub(crate) incoming: Decimal,
    /// Value leaving above the line.
    pub(crate) outgoing: Decimal,
    /// The newest block above the line, when one is held.
    pub(crate) pending: Option<BlockRef>,
}

/// The settled half of a balance, as one snapshot holds it.
pub(crate) struct SettledBalance {
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
    /// `of`'s settled balance, read inside the snapshot `connection` holds:
    /// the total indexing keeps for an observed holding, or every `ledger` row
    /// of it folded in Rust (amounts are u256-capable text, never summed by
    /// SQLite), with how far behind the snapshot's inspection is.
    ///
    /// # Errors
    ///
    /// A `Store` error when a read fails or a row is malformed.
    pub(crate) async fn read(
        connection: &mut SqliteConnection,
        of: &dyn Holder,
    ) -> Result<Self, IndexError> {
        let address = of.address();
        let Checkpoint(checkpoint) = Checkpoint::get(connection, &address.network).await?;
        let observations = Observation::find_by_address(connection, address).await?;
        let net = match observations.kept(of.asset()) {
            Some(net) => net,
            None => Self::fold(connection, address, &of.asset().asset).await?,
        };
        Ok(Self {
            net,
            behind: observations.behind(checkpoint.as_ref()),
            observed_at: checkpoint,
        })
    }

    /// The balance with what `sample`'s blocks above this read's line move.
    ///
    /// A movement between two of the holder's own endpoints is reported as
    /// both arriving and leaving: above the line the question is what is
    /// moving, not what the total will be. A block credit counts as arriving.
    #[must_use]
    pub(crate) fn completed(self, of: &dyn Holder, sample: Sample) -> Balance {
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

    /// Every `ledger` row of one holding, credits minus debits, exact.
    async fn fold(
        connection: &mut SqliteConnection,
        address: &Address,
        asset: &str,
    ) -> Result<BigInt, IndexError> {
        let rows = sqlx::query(
            "SELECT direction, amount FROM ledger
             WHERE chain = ?1 AND network = ?2 AND address = ?3 AND asset = ?4",
        )
        .bind(address.network.chain.as_str())
        .bind(&address.network.name)
        .bind(&address.value)
        .bind(asset)
        .fetch_all(connection)
        .await
        .map_err(|_| IndexError::new(IndexErrorKind::Store, "failed to read movements", true))?;
        let malformed = || {
            IndexError::new(
                IndexErrorKind::Store,
                "a stored movement is malformed",
                false,
            )
        };
        let (mut credits, mut debits) = (BigUint::ZERO, BigUint::ZERO);
        for row in rows {
            let direction: i64 = row.try_get("direction").map_err(|_| malformed())?;
            let amount: String = row.try_get("amount").map_err(|_| malformed())?;
            let units = amount
                .parse::<Decimal>()
                .ok()
                .and_then(|amount| amount.to_atomic(0).ok())
                .ok_or_else(malformed)?;
            match direction {
                0 => credits += units,
                1 => debits += units,
                _ => return Err(malformed()),
            }
        }
        Ok(BigInt::from(credits) - BigInt::from(debits))
    }
}
