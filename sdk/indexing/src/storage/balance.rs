//! One holding's balance: the settled fold, and what is still above the line.
//!
//! The confirmed half derives from stored movements. Shipped repositories
//! maintain watched totals atomically and use [`Fold`] for independent
//! verification and unwatched holdings. Every amount here is in the
//! asset's **atomic** units: this crate never learns an asset's decimals, so the object is built
//! by code that
//! physically cannot produce display units.
//!
//! The pending half is [`Balances`]: the blocks an index holds above the line,
//! folded over a sample taken before the repository is read ([`super::join`]).
//!
//! **The two folds deliberately disagree about a self-transfer.** The settled
//! fold nets a movement between two of the holder's own endpoints to nothing,
//! because its question is what the total is. The pending fold reports the same
//! movement as both arriving and leaving, because its question is what is
//! moving. Neither is a defect of the other, and they are not to be unified.

use num_bigint::BigUint;
#[cfg(any(feature = "sqlite", feature = "memory"))]
use num_bigint::{BigInt, Sign};

use crate::{
    Address, BalanceQuery, BalanceStore, BlockCredit, BlockRef, BlockWithOutputs, BoxFuture,
    Checkpointed, Decimal, FeeSettlement, Holder, IndexError, IndexErrorKind, PendingJoin,
    Transaction, ValueMovement,
};

/// One address's holding of one asset, on both sides of the confirmed line.
///
/// `amount` is settled: it is folded over durable rows only, so it cannot
/// revert. `pending` is what the blocks above the line move, reported in and
/// out rather than netted. The two halves carry their own anchors, so a
/// caller can tell how stale each one is without a second call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Balance {
    /// Confirmed indexed total in atomic units; completeness and spendability
    /// are separate claims.
    pub amount: Decimal,
    /// The confirmed line `amount` was read at; `None` before the
    /// network's first commit.
    pub observed_at: Option<BlockRef>,
    /// What the blocks above that line move, in and out, and the newest of
    /// them.
    pub pending: PendingValue,
    /// How much of the history behind `amount` the repository can prove it
    /// holds.
    pub completeness: Completeness,
}

/// The line the settled amount was folded at is the line a pending sample is
/// narrowed by: what is above it is what `amount` does not hold.
impl Checkpointed for Balance {
    fn checkpoint(&self) -> Option<&BlockRef> {
        self.observed_at.as_ref()
    }
}

/// Value in blocks above the confirmed line, for one address and one asset.
///
/// The two directions are reported apart rather than netted: a payments
/// operator reads "3.0 arriving, 1.0 leaving" as two facts, and
/// [`Decimal`](crate::Decimal) has no subtraction to net them with. Neither
/// side is ever negative.
///
/// Network fees are not folded in. A fee is reported with its transaction in
/// history, and on UTXO chains it is already the difference between the
/// spent inputs and the created outputs, so counting it again would be wrong
/// on one chain or missing on another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingValue {
    /// Value arriving at the address above the line.
    pub incoming: Decimal,
    /// Value leaving the address above the line.
    pub outgoing: Decimal,
    /// The newest pending block, or `None` when none is held.
    pub block: Option<BlockRef>,
}

impl PendingValue {
    /// Nothing above the line: what a store's own fold reports, before a
    /// consumer folds the pending blocks in.
    #[must_use]
    pub fn none() -> Self {
        Self {
            incoming: Decimal::zero(),
            outgoing: Decimal::zero(),
            block: None,
        }
    }
}

/// How much of the history behind a settled amount the repository can prove it
/// holds.
///
/// A fold is only as complete as the rows behind it: an address watched from a
/// birthday later than its first funding has credits the index never saw. There
/// is no way to bound that from inside the fold, so the answer is reported
/// rather than assumed.
///
/// **The repository answers it, from the snapshot the rows came from.** The
/// claim is a fact about the observations of the address — where each began,
/// and how far each has been inspected — and those are rows of the same store,
/// read at the same checkpoint as the answer they qualify
/// ([`crate::Inspection`]). Nothing downstream weakens or
/// strengthens it: an index does not police a repository's answer, and a
/// consumer that joins pending blocks to it passes the claim on unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completeness {
    /// Every change the address ever made is indexed, because observation
    /// started at or before its first use.
    ///
    /// A store earns this in exactly one case: the birthday is the chain's
    /// first block, progress has been recorded for it, **and** no observation
    /// of the address is still behind the confirmed line the answer was read
    /// at, so there is no block a credit could hide in. Both backends answer
    /// that case and no other.
    ///
    /// For a birthday above the first block the claim cannot be earned this
    /// way, and proving it needs a per-asset backfill.
    CompleteHistory,
    /// Every change since the birthday is indexed, and nothing proves the
    /// birthday was early enough. The honest answer for every chain today.
    Incomplete,
    /// The fold went negative, which a complete index cannot do: more left the
    /// address than ever reached it, so credits are provably missing.
    /// `amount` is zero, because no honest number exists.
    ///
    /// Signed persisted totals preserve this deficit; presentation alone renders
    /// zero. Independent verification also detects disagreement with stored effects.
    Overdrawn,
}

/// How one movement moves one address: a credit, a debit, or nothing.
///
/// The sign belongs to the `(movement, address)` pair rather than to the
/// movement, which is why it is projected here and never stored: stored
/// amounts are non-negative by an invariant checked at three layers, and a
/// second signed copy could disagree with the first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MovementEffect {
    /// Value reached the address.
    Credit(Decimal),
    /// Value left the address.
    Debit(Decimal),
    /// The movement does not touch the address, or both of its endpoints are
    /// the address and it nets to zero.
    None,
}

impl ValueMovement {
    /// How this movement moves `address`.
    ///
    /// A self-transfer is [`MovementEffect::None`]: both endpoints are the address,
    /// so the holding is unchanged. (A *pending* read reports the same
    /// movement as both arriving and leaving, because there the question is
    /// what is moving, not what the total will be.)
    #[must_use]
    pub fn effect(&self, address: &Address) -> MovementEffect {
        let endpoint = |value: Option<&Address>| value == Some(address);
        match (endpoint(self.to()), endpoint(self.from())) {
            (true, false) => MovementEffect::Credit(self.amount.clone()),
            (false, true) => MovementEffect::Debit(self.amount.clone()),
            (true, true) | (false, false) => MovementEffect::None,
        }
    }
}

/// A running total of one holding's credits and debits, in atomic units.
///
/// [`Decimal`] has no subtraction by design, so the fold runs in `BigUint`
/// through the public `to_atomic(0)` / `from_atomic(_, 0)` pair: every stored
/// movement is already exact at scale 0, so the conversion is lossless and
/// the arithmetic is independent of the asset's decimals.
///
/// A store builds one per query, applies every stored transaction of the
/// address to it in any order, and asks for the [`Balance`].
pub struct Fold<'a> {
    of: &'a dyn Holder,
    credits: BigUint,
    debits: BigUint,
}

impl<'a> Fold<'a> {
    /// An empty total for `of`.
    ///
    /// No chain rule, because the rule is no longer a reader's to be told: where
    /// a network fee settles is decided once, at commit, and the answer is among
    /// the rows this folds ([`FeeSettlement`]).
    #[must_use]
    pub fn new(of: &'a dyn Holder) -> Self {
        Self {
            of,
            credits: BigUint::ZERO,
            debits: BigUint::ZERO,
        }
    }

    /// Restores a persisted signed atomic total, preserving an unresolved deficit.
    #[cfg(any(feature = "sqlite", feature = "memory"))]
    pub(crate) fn from_net(of: &'a dyn Holder, net: BigInt) -> Self {
        let (sign, units) = net.into_parts();
        let (credits, debits) = if sign == Sign::Minus {
            (BigUint::ZERO, units)
        } else {
            (units, BigUint::ZERO)
        };
        Self {
            of,
            credits,
            debits,
        }
    }

    /// The exact net, including debits whose earlier credits are not indexed yet.
    #[cfg(feature = "memory")]
    pub(crate) fn net(self) -> BigInt {
        BigInt::from(self.credits) - BigInt::from(self.debits)
    }

    /// Adds one stored effect.
    ///
    /// The primitive the whole fold is built out of, and the only one a store
    /// that keeps effects rather than transactions needs: the sign is the row's
    /// own, so nothing is projected here and no chain rule is consulted.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::Store`] when a stored amount is not a non-negative
    /// integer, which the write path forbids.
    pub fn record(&mut self, effect: &MovementEffect) -> Result<(), IndexError> {
        match effect {
            MovementEffect::Credit(amount) => self.credits += Self::units(amount)?,
            MovementEffect::Debit(amount) => self.debits += Self::units(amount)?,
            MovementEffect::None => {}
        }
        Ok(())
    }

    /// Adds every effect `transaction` has on the holding.
    ///
    /// Movements of another asset are skipped, and so is a fee this chain
    /// already settled inside its movements, one another address paid, or one
    /// charged in an asset this holding is not read in.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::Store`] when a stored amount is not a non-negative
    /// integer, which the write path forbids.
    pub fn apply(
        &mut self,
        transaction: &Transaction,
        settlement: FeeSettlement,
    ) -> Result<(), IndexError> {
        for movement in &transaction.movements {
            if &movement.asset != self.of.asset() {
                continue;
            }
            self.record(&movement.effect(self.of.address()))?;
        }
        let charged = transaction.fee.as_ref().filter(|fee| {
            settlement == FeeSettlement::Charged
                && fee.payer.as_ref() == Some(self.of.address())
                && &fee.asset == self.of.asset()
        });
        if let Some(fee) = charged {
            self.record(&MovementEffect::Debit(fee.amount.clone()))?;
        }
        Ok(())
    }

    /// Applies one transactionless credit for this holding. Other addresses or assets
    /// are ignored. Credits are separate from transaction movements and fees, so both
    /// may appear in one block without double-counting. Stores enforce credit identity.
    ///
    /// # Errors
    /// A store error when the credited amount is not an atomic nonnegative integer.
    pub fn apply_credit(&mut self, credit: &BlockCredit) -> Result<(), IndexError> {
        if &credit.asset == self.of.asset() && &credit.to == self.of.address() {
            self.record(&MovementEffect::Credit(credit.amount.clone()))?;
        }
        Ok(())
    }

    /// The settled balance, folded at `observed_at`, with nothing pending.
    ///
    /// A total that would go negative is not an error to swallow: a complete
    /// index cannot spend more than it received, so it is the cheapest
    /// available proof that the index is missing credits, and it is reported
    /// as [`Completeness::Overdrawn`].
    #[must_use]
    pub fn balance(self, observed_at: Option<BlockRef>) -> Balance {
        let (amount, completeness) = if self.credits < self.debits {
            (Decimal::zero(), Completeness::Overdrawn)
        } else {
            (
                Decimal::from_atomic(self.credits - self.debits, 0),
                Completeness::Incomplete,
            )
        };
        Balance {
            amount,
            observed_at,
            pending: PendingValue::none(),
            completeness,
        }
    }

    /// One stored amount as atomic units.
    fn units(amount: &Decimal) -> Result<BigUint, IndexError> {
        amount.to_atomic(0).map_err(|error| {
            IndexError::new(
                IndexErrorKind::Store,
                format!("stored amount is not a non-negative integer: {error}"),
                false,
            )
        })
    }
}

/// Reads one holding's balance: the settled fold with the pending half
/// folded in.
///
/// No repository answers it alone: `pending` is what an index holds above its
/// confirmed line ([`crate::Pending`]). [`PendingJoin`] composes the two.
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
    /// Deliberately not the settled fold's netting (`crate::MovementEffect`): a
    /// movement between two of the holder's own endpoints is reported as both
    /// arriving and leaving here, because the question above the line is what
    /// is moving, where a balance's question is what the total will be.
    ///
    /// A block credit counts as arriving, for the same reason it counts in
    /// the settled fold: it is value on its way to the address. It is the one
    /// thing reported here that the pending half of a *history* page cannot
    /// show: a credit is not a transaction. Leaving it out would understate
    /// an address for the whole depth of the confirmed line.
    fn moving(of: &dyn Holder, blocks: &[BlockWithOutputs]) -> PendingValue {
        let movements = blocks
            .iter()
            .flat_map(|facts| facts.block.transactions())
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
            .flat_map(|facts| facts.credits.as_slice())
            .filter(|credit| &credit.asset == of.asset() && &credit.to == of.address());
        incoming.extend(credits.map(|credit| credit.amount.clone()));
        PendingValue {
            incoming: Decimal::total(&incoming),
            outgoing: Decimal::total(&outgoing),
            block: blocks.last().map(|held| held.block.reference().clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use base::ChainName;

    use super::*;
    use crate::{
        AssetId, BlockHash, BlockHeight, BlockPosition, MovementId, Network, NetworkFee, Status,
        Subject, TransactionRef,
    };

    fn network() -> Network {
        Network::new(ChainName::new("test"), "mainnet")
    }

    fn native() -> AssetId {
        AssetId::native(network().chain)
    }

    fn subject(value: &str) -> Subject {
        Subject {
            address: Address::new(network(), value),
            asset: native(),
        }
    }

    fn block() -> BlockRef {
        BlockRef {
            position: BlockPosition(1),
            height: BlockHeight(1),
            hash: BlockHash(vec![1]),
            parent: None,
            timestamp: None,
        }
    }

    /// A transfer of `amount` to `to`, with `payer` charged a fee of 3.
    fn transaction(to: &str, amount: u64, payer: &str) -> Transaction {
        Transaction {
            id: TransactionRef::new(network(), "transaction"),
            movements: vec![ValueMovement::transfer(
                MovementId("value".into()),
                native(),
                Decimal::from(amount),
                Address::new(network(), "sender"),
                Address::new(network(), to),
            )],
            fee: Some(NetworkFee {
                asset: native(),
                amount: Decimal::from(3_u64),
                payer: Some(Address::new(network(), payer)),
            }),
            order: 0,
            status: Status::Included { block: block() },
        }
    }

    fn credit(id: &str, to: &str, asset: AssetId, amount: u64) -> BlockCredit {
        BlockCredit {
            id: MovementId(id.into()),
            asset,
            amount: Decimal::from(amount),
            to: Address::new(network(), to),
        }
    }

    /// A block's transaction movement and independent credit each count once.
    #[test]
    fn a_credit_and_a_transaction_to_one_address_are_each_counted_once() {
        let holder = subject("beneficiary");
        let paid = transaction("beneficiary", 40, "sender");
        let given = credit("withdrawal:1", "beneficiary", native(), 2);

        let mut both = Fold::new(&holder);
        both.apply(&paid, FeeSettlement::Charged)
            .expect("a valid transaction");
        both.apply_credit(&given).expect("a valid credit");
        assert_eq!(both.balance(None).amount, Decimal::from(42_u64));

        // Each half alone, so the total above is provably the sum of two
        // facts rather than either one doubled.
        let mut alone = Fold::new(&holder);
        alone
            .apply(&paid, FeeSettlement::Charged)
            .expect("a valid transaction");
        assert_eq!(alone.balance(None).amount, Decimal::from(40_u64));

        let mut credited = Fold::new(&holder);
        credited.apply_credit(&given).expect("a valid credit");
        assert_eq!(credited.balance(None).amount, Decimal::from(2_u64));
    }

    /// A credit names an asset and an address, and the fold honours both:
    /// gas is charged in the native asset, so a token holding at the same
    /// address must not gain the native credit, and no other address may.
    #[test]
    fn a_credit_reaches_only_the_holding_it_names() {
        let token = AssetId {
            chain: network().chain,
            asset: "token".into(),
        };
        let native_credit = credit("withdrawal:1", "beneficiary", native(), 9);
        let token_credit = credit("withdrawal:2", "beneficiary", token.clone(), 5);
        let elsewhere = credit("withdrawal:3", "stranger", native(), 100);

        let holder = subject("beneficiary");
        let mut ether = Fold::new(&holder);
        for given in [&native_credit, &token_credit, &elsewhere] {
            ether.apply_credit(given).expect("a valid credit");
        }
        assert_eq!(ether.balance(None).amount, Decimal::from(9_u64));

        let subject = Subject {
            address: Address::new(network(), "beneficiary"),
            asset: token,
        };
        let mut held = Fold::new(&subject);
        for given in [&native_credit, &token_credit, &elsewhere] {
            held.apply_credit(given).expect("a valid credit");
        }
        assert_eq!(held.balance(None).amount, Decimal::from(5_u64));
    }

    /// A credit can only settle a proof of incompleteness, never create one:
    /// it enters the same total a movement's credit does.
    #[test]
    fn a_credit_discharges_an_overdrawn_fold_instead_of_masking_it() {
        let holder = subject("spender");
        let mut spent = transaction("elsewhere", 0, "spender");
        spent.movements.clear();

        let mut short = Fold::new(&holder);
        short
            .apply(&spent, FeeSettlement::Charged)
            .expect("a valid transaction");
        assert_eq!(short.balance(None).completeness, Completeness::Overdrawn);

        let mut settled = Fold::new(&holder);
        settled
            .apply(&spent, FeeSettlement::Charged)
            .expect("a valid transaction");
        settled
            .apply_credit(&credit("withdrawal:1", "spender", native(), 3))
            .expect("a valid credit");
        let balance = settled.balance(None);
        assert_eq!(balance.amount, Decimal::zero());
        assert_eq!(balance.completeness, Completeness::Incomplete);
    }
}

#[cfg(test)]
mod joined {
    use crate::{AssetId, BlockCredit, BlockWithOutputs, HistoryQuery, MovementId, Subject};

    use super::*;
    use crate::storage::join::tests::{
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
    /// with nothing in the answer identifying the missing value.
    #[test]
    fn a_block_credit_above_the_line_is_pending_incoming() {
        let holder = Subject {
            address: address("beneficiary"),
            asset: AssetId::native(network().chain),
        };
        let interpreted = BlockWithOutputs {
            credits: vec![BlockCredit {
                id: MovementId("withdrawal:1".into()),
                asset: holder.asset.clone(),
                amount: Decimal::from(2_u64),
                to: holder.address.clone(),
            }],
            ..BlockWithOutputs::new(&network(), block(1), Vec::new()).expect("an empty block")
        };
        let held = vec![interpreted];

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
