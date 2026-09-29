//! One holding's balance: the settled fold, and what is still above the line.
//!
//! The confirmed half is a fold over the movements the index already stores
//! ([`Ledger`]), not a second number read from a node, so it cannot disagree
//! with the history the same store answers. Every amount here is in the
//! asset's **atomic** units: this crate never learns an asset's decimals
//! (`docs/design/021` section 5.2), so the object is built by code that
//! physically cannot produce display units.

use num_bigint::BigUint;

use crate::{
    Address, BlockCredit, BlockRef, BoxFuture, CanonicalTransaction, Decimal, Holder, IndexError,
    IndexErrorKind, PendingValue, ValueMovement,
};

/// One address's holding of one asset, on both sides of the confirmed line.
///
/// `amount` is settled: it is folded over durable rows only, so it cannot
/// revert. `pending` is what the blocks above the line move, reported in and
/// out rather than netted. The two halves carry their own anchors, so a
/// caller can tell how stale each one is without a second call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Balance {
    /// Settled and spendable, in atomic units.
    pub amount: Decimal,
    /// The confirmed line `amount` was folded at; `None` before the
    /// network's first commit.
    pub observed_at: Option<BlockRef>,
    /// What the blocks above that line move, in and out, and the newest of
    /// them.
    pub pending: PendingValue,
    /// How much of the history behind `amount` the index can prove it holds.
    pub completeness: Completeness,
}

/// How much of the history behind a settled amount the index can prove it
/// holds.
///
/// A fold is only as complete as the rows behind it: an address watched from a
/// birthday later than its first funding has credits the index never saw. There
/// is no way to bound that from inside the fold, so the answer is reported
/// rather than assumed.
///
/// **Two axes, not one, and the names used to hide it.** The variant formerly
/// called `Complete` claimed the birthday was proven early enough, while the
/// only thing that ever constructed it — an independent node read agreeing with
/// the fold at one block — proves the *total* at that block and says nothing
/// about the birthday. The two are now named apart: [`Self::VerifiedOpening`]
/// is what the agreement proves, and [`Self::CompleteHistory`] is what the name
/// claimed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completeness {
    /// Every change the address ever made is indexed, because observation
    /// started at or before its first use.
    ///
    /// A store earns this in exactly one case: the birthday is the chain's
    /// first block **and** progress has reached it, so there is no earlier
    /// block a credit could hide in. Both backends answer that case and no
    /// other.
    ///
    /// For a birthday above the first block the claim cannot be earned this
    /// way, and proving it needs a per-asset backfill (`docs/design/021`
    /// section 7.4, `docs/design/030` step 13). The index **refuses to pass it
    /// on** there ([`Self::without_a_birthday_proof`]), so a store that claims
    /// it without having earned it cannot reach a caller.
    CompleteHistory,
    /// The settled total was checked against an independent read of the same
    /// holding at the same block, and the two agreed.
    ///
    /// This is what `chain_ethereum::BalanceWitness` actually proves, and it is
    /// **not durable**: the witness reads a node and mutates a [`Balance`] on
    /// its way to a caller, so no store can write it and none carries a column
    /// for it. Persisting it means giving the witness a store, which is an
    /// ownership change rather than a schema one. It is a claim about the
    /// *total*, not about the birthday: a fold short by a pre-birthday credit
    /// and a node that agrees with it cannot both be right, which is why the
    /// agreement is worth recording and why it is not [`Self::CompleteHistory`].
    VerifiedOpening,
    /// Every change since the birthday is indexed, and nothing proves the
    /// birthday was early enough. The honest answer for every chain today.
    Incomplete,
    /// The fold went negative, which a complete index cannot do: more left the
    /// address than ever reached it, so credits are provably missing.
    /// `amount` is zero, because no honest number exists.
    ///
    /// **Only a recomputation can produce it.** A total maintained forward from
    /// what each commit inserted has no way to notice it is short — it clamps at
    /// zero and looks like an honest small balance — so this variant is the one
    /// thing that keeps the fold worth running (`docs/design/034` section 5.3).
    Overdrawn,
}

impl Completeness {
    /// This claim, weakened by history the index has not finished reading.
    ///
    /// A fold is a fold over rows, and a backfill exists precisely because rows
    /// are missing: the range below what has been inspected has not been read
    /// for the address yet, so no scan of what is stored can prove anything
    /// stronger than [`Self::Incomplete`].
    ///
    /// [`Self::Overdrawn`] survives: it is a proof rather than a claim — the
    /// stored debits already exceed the stored credits — and a backfill only
    /// ever adds credits, so the proof cannot become less true by being
    /// weakened.
    #[must_use]
    pub const fn while_backfilling(self) -> Self {
        match self {
            Self::CompleteHistory | Self::VerifiedOpening | Self::Incomplete => Self::Incomplete,
            Self::Overdrawn => Self::Overdrawn,
        }
    }

    /// This claim, weakened by an observation that did not start at the chain's
    /// first block.
    ///
    /// The other axis, and the one no store can be trusted on:
    /// [`Self::CompleteHistory`] says every change the address ever made is
    /// indexed, and an observation born above the first block has by
    /// construction not read the range below it — a range no backfill will ever
    /// cover, because a backfill reads *from* the birthday. So the index checks
    /// the one thing it can check, and a store claiming more than its own watch
    /// row can support is answered rather than believed.
    ///
    /// [`Self::VerifiedOpening`] is untouched, and that is the whole reason the
    /// two are separate variants: an agreement at one block is evidence about
    /// the total that survives an arbitrary birthday, and weakening it here
    /// would throw away the only positive proof any chain produces.
    #[must_use]
    pub const fn without_a_birthday_proof(self) -> Self {
        match self {
            Self::CompleteHistory => Self::Incomplete,
            other => other,
        }
    }

    /// This claim, narrowed for a read that reports no amount.
    ///
    /// The third axis, and the only one that is a fact about the *answer*
    /// rather than about the address: [`Self::VerifiedOpening`] says an
    /// independent read of one holding at one block agreed with the fold of the
    /// same rows, which is evidence about a total. A
    /// [`CanonicalPage`](crate::CanonicalPage) reports which transactions an
    /// address took part in and no total at all, so the evidence has nothing on
    /// it to be evidence about, and the page falls back to the range claim it
    /// can answer.
    ///
    /// The range claims pass through untouched, because a page is exactly a
    /// window onto the range they speak about. So does [`Self::Overdrawn`],
    /// which is the fold's own proof and reaches no page in the first place —
    /// a store that let it would already be answering the two reads
    /// differently, which `contracts::HistoryContract` refuses on its own.
    #[must_use]
    pub const fn without_a_reported_amount(self) -> Self {
        match self {
            Self::VerifiedOpening => Self::Incomplete,
            other => other,
        }
    }
}

/// Where a chain settles its network fee, relative to the movements its
/// interpreter emits.
///
/// The distinction is not cosmetic and it is not guessable. On a UTXO chain
/// the interpreter emits every spent input and every created output, so
/// `Σ inputs − Σ outputs` has already paid the fee; subtracting
/// [`NetworkFee`](crate::NetworkFee) again would charge it twice. On an
/// account chain the fee is charged outside the transferred value and has to
/// be applied, to its payer and in the fee asset only — which is why a token
/// holding never pays gas, and the same address's native holding always does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeeSettlement {
    /// Already inside the movements: the fold must not apply it again.
    Movements,
    /// Charged beside them: the fold debits the payer, in the fee asset only.
    Charged,
}

/// How one movement moves one address: a credit, a debit, or nothing.
///
/// The sign belongs to the `(movement, address)` pair rather than to the
/// movement, which is why it is projected here and never stored: stored
/// amounts are non-negative by an invariant checked at three layers, and a
/// second signed copy could disagree with the first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
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
    /// A self-transfer is [`Effect::None`]: both endpoints are the address,
    /// so the holding is unchanged. (A *pending* read reports the same
    /// movement as both arriving and leaving, because there the question is
    /// what is moving, not what the total will be.)
    #[must_use]
    pub fn effect(&self, address: &Address) -> Effect {
        let endpoint = |value: Option<&Address>| value == Some(address);
        match (endpoint(self.to()), endpoint(self.from())) {
            (true, false) => Effect::Credit(self.amount.clone()),
            (false, true) => Effect::Debit(self.amount.clone()),
            (true, true) | (false, false) => Effect::None,
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
pub struct Ledger<'a> {
    of: &'a dyn Holder,
    credits: BigUint,
    debits: BigUint,
}

impl<'a> Ledger<'a> {
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
    pub fn record(&mut self, effect: &Effect) -> Result<(), IndexError> {
        match effect {
            Effect::Credit(amount) => self.credits += Self::units(amount)?,
            Effect::Debit(amount) => self.debits += Self::units(amount)?,
            Effect::None => {}
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
        transaction: &CanonicalTransaction,
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
            self.record(&Effect::Debit(fee.amount.clone()))?;
        }
        Ok(())
    }

    /// Adds a credit the block made with no transaction to name it.
    ///
    /// Unconditional: a [`BlockCredit`] of the holding's own asset, to the
    /// holding's own address, is value that reached it, and there is no
    /// counterparty, no fee and no failure status to weigh. A credit of
    /// another asset or to another address is skipped, exactly as a movement
    /// of another asset is.
    ///
    /// It cannot double-count against a transaction that credited the same
    /// address in the same block, because the two are disjoint by
    /// construction: [`apply`](Self::apply) reads only
    /// [`CanonicalTransaction::movements`] and the transaction's fee, a
    /// credit is in neither, and no chain can express one as the other —
    /// which is precisely why `docs/design/021` section 4.4 refused the
    /// synthetic transaction identifier. Applying the *same* credit twice is
    /// prevented where identity lives: `BlockCredit::validate_all` rejects a
    /// repeated identity inside a block, and a store keys the row by
    /// `(address, height, id)`.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::Store`] when a stored amount is not a non-negative
    /// integer, which the write path forbids.
    pub fn apply_credit(&mut self, credit: &BlockCredit) -> Result<(), IndexError> {
        if &credit.asset == self.of.asset() && &credit.to == self.of.address() {
            self.record(&Effect::Credit(credit.amount.clone()))?;
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
/// The subject is a [`Holder`], so a wallet answers it and so does a watched
/// address with no key behind it ([`Subject`](crate::Subject)) — a fold over
/// indexed movements needs no key.
pub trait Balances: Send + Sync {
    /// The balance of `of`, on both sides of its chain's confirmed line.
    fn balance<'a>(&'a self, of: &'a dyn Holder) -> BoxFuture<'a, Result<Balance, IndexError>>;
}

#[cfg(test)]
mod tests {
    use base::ChainName;

    use super::*;
    use crate::{
        AssetId, BlockHash, BlockHeight, BlockPosition, CanonicalStatus, MovementId, Network,
        NetworkFee, Subject, TransactionRef,
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
    fn transaction(to: &str, amount: u64, payer: &str) -> CanonicalTransaction {
        CanonicalTransaction {
            transaction_id: TransactionRef::new(network(), "transaction"),
            transaction_order: 0,
            status: CanonicalStatus::Included { block: block() },
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

    /// The constraint `docs/design/021` section 4.4 exists to guarantee: a
    /// block that both pays an address by transaction and credits it from its
    /// own body counts each fact once.
    ///
    /// The two cannot be confused because they are different types read from
    /// different collections — which is what the rejected synthetic
    /// transaction identifier would have destroyed, by making a credit
    /// indistinguishable from a transaction to every reader and every join.
    #[test]
    fn a_credit_and_a_transaction_to_one_address_are_each_counted_once() {
        let holder = subject("beneficiary");
        let paid = transaction("beneficiary", 40, "sender");
        let given = credit("withdrawal:1", "beneficiary", native(), 2);

        let mut both = Ledger::new(&holder);
        both.apply(&paid, FeeSettlement::Charged)
            .expect("a valid transaction");
        both.apply_credit(&given).expect("a valid credit");
        assert_eq!(both.balance(None).amount, Decimal::from(42_u64));

        // Each half alone, so the total above is provably the sum of two
        // facts rather than either one doubled.
        let mut alone = Ledger::new(&holder);
        alone
            .apply(&paid, FeeSettlement::Charged)
            .expect("a valid transaction");
        assert_eq!(alone.balance(None).amount, Decimal::from(40_u64));

        let mut credited = Ledger::new(&holder);
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
        let mut ether = Ledger::new(&holder);
        for given in [&native_credit, &token_credit, &elsewhere] {
            ether.apply_credit(given).expect("a valid credit");
        }
        assert_eq!(ether.balance(None).amount, Decimal::from(9_u64));

        let subject = Subject {
            address: Address::new(network(), "beneficiary"),
            asset: token,
        };
        let mut held = Ledger::new(&subject);
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
        let spent = CanonicalTransaction {
            movements: Vec::new(),
            ..transaction("elsewhere", 0, "spender")
        };

        let mut short = Ledger::new(&holder);
        short
            .apply(&spent, FeeSettlement::Charged)
            .expect("a valid transaction");
        assert_eq!(short.balance(None).completeness, Completeness::Overdrawn);

        let mut settled = Ledger::new(&holder);
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
