//! One holding's balance on both sides of the confirmed line: the capability a
//! consumer declares, and the join that answers it.
//!
//! The settled half is the repository's own fold over its durable rows, with
//! the claim it makes about the history behind them. The pending half is what
//! the blocks above the line move, folded here from a sample of them, in the
//! order [`crate::pending`] states.
//!
//! **The two folds deliberately disagree about a self-transfer.** The settled
//! fold nets a movement between two of the holder's own endpoints to nothing,
//! because its question is what the total is. The pending fold reports the same
//! movement as both arriving and leaving, because its question is what is
//! moving. Neither is a defect of the other, and they are not to be unified.

use base::BoxFuture;
use indexing::{
    Address, Balance, BalanceQuery, BalanceStore, CanonicalBlock, Decimal, Holder, IndexError,
    PendingValue,
};

use crate::PendingJoin;

/// Reads one holding's balance: the settled fold with the pending half
/// folded in.
///
/// No repository answers it alone: `pending` is what an index holds above its
/// confirmed line ([`indexing::Pending`]). [`PendingJoin`] composes the two.
///
/// The subject is a [`Holder`], so a wallet answers it and so does a watched
/// address with no key behind it ([`crate::Subject`]) — a fold over
/// indexed movements needs no key.
pub trait Balances: Send + Sync {
    /// The balance of `of`, on both sides of its chain's confirmed line.
    fn balance<'a>(&'a self, of: &'a dyn Holder) -> BoxFuture<'a, Result<Balance, IndexError>>;
}

impl<R: BalanceStore> Balances for PendingJoin<R> {
    fn balance<'a>(&'a self, of: &'a dyn Holder) -> BoxFuture<'a, Result<Balance, IndexError>> {
        Box::pin(async move {
            let query = BalanceQuery::new(of);
            query.validate()?;
            let sample = self.pending.sampled()?;
            let mut balance = self.repository.balance(query).await?;
            let pending = sample.above(&balance);
            balance.pending = Self::moving(of, &pending);
            Ok(balance)
        })
    }
}

impl<R> PendingJoin<R> {
    /// The value `of`'s address and asset move in `blocks`.
    ///
    /// Deliberately not the settled fold's netting (`indexing::MovementEffect`): a
    /// movement between two of the holder's own endpoints is reported as both
    /// arriving and leaving here, because the question above the line is what
    /// is moving, where a balance's question is what the total will be.
    ///
    /// A block credit counts as arriving, for the same reason it counts in
    /// the settled fold: it is value on its way to the address. It is the one
    /// thing reported here that the pending half of a *history* page cannot
    /// show, because that page lists transactions and a credit is not one
    /// (`docs/design/021` section 4.4). Leaving it out instead would understate
    /// an address for the whole depth of the confirmed line.
    fn moving(of: &dyn Holder, blocks: &[CanonicalBlock]) -> PendingValue {
        let movements = blocks
            .iter()
            .flat_map(CanonicalBlock::transactions)
            .flat_map(|transaction| &transaction.movements)
            .filter(|movement| &movement.asset == of.asset());
        let (mut incoming, mut outgoing) = (Vec::new(), Vec::new());
        for movement in movements {
            let at = |endpoint: Option<&Address>| endpoint == Some(of.address());
            incoming.extend(at(movement.to()).then(|| movement.amount.clone()));
            outgoing.extend(at(movement.from()).then(|| movement.amount.clone()));
        }
        let credits = blocks
            .iter()
            .flat_map(CanonicalBlock::credits)
            .filter(|credit| &credit.asset == of.asset() && &credit.to == of.address());
        incoming.extend(credits.map(|credit| credit.amount.clone()));
        PendingValue {
            incoming: Decimal::total(&incoming),
            outgoing: Decimal::total(&outgoing),
            block: blocks.last().map(|held| held.block().clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use indexing::{
        AssetId, BlockCredit, CanonicalBlock, HistoryQuery, InterpretedBlock, MovementId, Subject,
    };

    use super::*;
    use crate::pending::tests::{
        Landing, Paged, address, admitted, block, interleaved, network, owner, query,
    };

    /// The pending fold and the settled fold deliberately disagree about a
    /// movement between the holder's own endpoints, and this is the pending
    /// half: both directions are reported, and nothing is netted.
    #[test]
    fn pending_value_reports_both_directions_without_netting_them() {
        let pending = [admitted(4, 5, "owner"), admitted(5, 7, "payer")];
        let value = PendingJoin::<Paged>::moving(&query(None), &pending);
        assert_eq!(value.incoming, Decimal::from(5_u64));
        assert_eq!(value.outgoing, Decimal::zero());
        assert_eq!(value.block, Some(block(5)));

        let payer = HistoryQuery {
            address: address("payer"),
            ..query(None)
        };
        let value = PendingJoin::<Paged>::moving(&payer, &pending);
        assert_eq!(value.incoming, Decimal::from(7_u64));
        assert_eq!(value.outgoing, Decimal::from(12_u64));
    }

    /// A credit above the confirmed line is value on its way, so it is
    /// reported as pending rather than withheld until the block settles.
    ///
    /// Left out, an address would read short for the whole depth of the line —
    /// twelve blocks on a typical Ethereum deployment — with nothing saying
    /// so, which is the drift `docs/design/021` section 4.1 exists to stop.
    #[test]
    fn a_block_credit_above_the_line_is_pending_incoming() {
        let holder = Subject {
            address: address("beneficiary"),
            asset: AssetId::native(network().chain),
        };
        let interpreted = InterpretedBlock {
            credits: vec![BlockCredit {
                id: MovementId("withdrawal:1".into()),
                asset: holder.asset.clone(),
                amount: Decimal::from(2_u64),
                to: holder.address.clone(),
            }],
            ..InterpretedBlock::new(&network(), block(1), Vec::new()).expect("an empty block")
        };
        let held = vec![CanonicalBlock::new(network(), interpreted).expect("a valid block")];

        let pending = PendingJoin::<Paged>::moving(&holder, &held);

        assert_eq!(pending.incoming, Decimal::from(2_u64));
        assert_eq!(pending.outgoing, Decimal::zero());
    }

    /// The two halves of a balance are one number: a credit the commit moved
    /// across the line is counted exactly once, on whichever side holds it.
    #[test]
    fn a_commit_landing_inside_a_read_moves_a_credit_across_the_line_without_losing_it() {
        for landing in Landing::ALL {
            let join = interleaved(landing);
            let balance = futures_executor::block_on(join.balance(&owner())).expect("balance");

            assert_eq!(
                Decimal::total([&balance.amount, &balance.pending.incoming]),
                Decimal::from(110_u64),
                "{landing:?}: settled plus pending is the same total wherever the commit lands"
            );
            assert_eq!(balance.pending.outgoing, Decimal::zero());
        }
    }
}
