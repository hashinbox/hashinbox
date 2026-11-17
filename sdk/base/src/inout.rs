//! Inputs and outputs of UTXO chains: what a transaction spends and what it
//! creates, each with its collection.

use crate::{Address, Addressed, Asset, Decimal, Future, Transacted, TransactionId};

/// Where a UTXO is: the output a `Received` leg created.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Coin {
    /// The creating transaction.
    pub transaction: TransactionId,
    /// The output's index in it (Bitcoin: the vout).
    pub index: u32,
}

impl Transacted for Coin {
    fn txid(&self) -> &TransactionId {
        &self.transaction
    }
}

/// An unspent output that a transaction can spend: a mined output with its
/// coin and height. `A` is the owner's address type: [`Address`], or a
/// chain's own address carrying what the chain needs (a script).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input<A: Addressed = Address> {
    /// Where it is.
    pub coin: Coin,
    /// Its owner.
    pub address: A,
    /// Its asset.
    pub asset: Asset,
    /// Atomic amount.
    pub amount: Decimal,
    /// Height of its creating block.
    pub height: u64,
}

/// An output a transaction creates: to whom and how much. It has no coin
/// until the transaction is mined; then it is an [`Input`] of its owner.
/// `A` is the recipient's address type, as for [`Input`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output<A: Addressed = Address> {
    /// The recipient.
    pub address: A,
    /// The asset.
    pub asset: Asset,
    /// Atomic amount.
    pub amount: Decimal,
}

/// Spendable inputs, with what coin selection needs: their total, an order,
/// and a filter that sees what it kept so far.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inputs<A: Addressed = Address>(Vec<Input<A>>);

impl<A: Addressed> Default for Inputs<A> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<A: Addressed + Clone> Inputs<A> {
    /// Their summed amount.
    #[must_use]
    pub fn total(&self) -> Decimal {
        Decimal::total(self.0.iter().map(|input| &input.amount))
    }

    /// The same inputs, largest amount first.
    #[must_use]
    pub fn largest_first(mut self) -> Self {
        self.0.sort_by(|left, right| right.amount.cmp(&left.amount));
        self
    }

    /// The inputs `keep` accepts, in order. `keep` sees what is kept so far
    /// and the next candidate, so it can select, for example: keep adding
    /// while the kept total does not yet cover the payment and the fee of
    /// that many inputs.
    ///
    /// ```ignore
    /// let chosen = inputs
    ///     .largest_first()
    ///     .filter(|kept, _| kept.total() < &amount + fee(kept.len() + 1));
    /// ```
    #[must_use]
    pub fn filter(&self, mut keep: impl FnMut(&Self, &Input<A>) -> bool) -> Self {
        let mut kept = Self::default();
        for input in &self.0 {
            if keep(&kept, input) {
                kept.0.push(input.clone());
            }
        }
        kept
    }
}

impl<A: Addressed> From<Vec<Input<A>>> for Inputs<A> {
    fn from(inputs: Vec<Input<A>>) -> Self {
        Self(inputs)
    }
}

impl<A: Addressed> std::ops::Deref for Inputs<A> {
    type Target = [Input<A>];

    fn deref(&self) -> &[Input<A>] {
        &self.0
    }
}

impl<A: Addressed> IntoIterator for Inputs<A> {
    type Item = Input<A>;
    type IntoIter = std::vec::IntoIter<Input<A>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// Where spendable inputs are read.
pub trait Store: Send + Sync {
    /// Every unspent output of `addresses`, as inputs to spend, in one read;
    /// each input names its address.
    ///
    /// # Errors
    ///
    /// When the store cannot read.
    fn find_unspent<'a>(&'a self, addresses: &'a [Address]) -> Future<'a, anyhow::Result<Inputs>>;
}
