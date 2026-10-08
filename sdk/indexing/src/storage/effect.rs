//! What indexed facts do to one holding: a signed amount in the asset's
//! **atomic** units — positive a credit, negative a debit, zero nothing.
//!
//! This crate never learns an asset's decimals, so every amount here is exact
//! at scale 0 and the arithmetic is independent of them. A deficit stays
//! negative: building a balance from a net, and saying what a negative one
//! means, is the reader's business.

use num_bigint::BigInt;

use crate::{
    Address, BlockCredit, Decimal, FeeSettlement, Holder, IndexError, IndexErrorKind, Transaction,
    Transactions, ValueMovement,
};

impl ValueMovement {
    /// What this movement does to `address`: its amount to the recipient, its
    /// negation to the sender, and zero to a self-transfer or a stranger.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::Store`] when the amount is not a non-negative
    /// integer, which the write path forbids.
    pub fn effect(&self, address: &Address) -> Result<BigInt, IndexError> {
        let endpoint = |value: Option<&Address>| value == Some(address);
        match (endpoint(self.to()), endpoint(self.from())) {
            (true, false) => units(&self.amount),
            (false, true) => units(&self.amount).map(|amount| -amount),
            (true, true) | (false, false) => Ok(BigInt::ZERO),
        }
    }
}

impl Transaction {
    /// What this transaction does to `of`: its movements in `of`'s asset, and
    /// the fee `of` paid when `settlement` charges it beside the movements and
    /// in `of`'s asset.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::Store`] when an amount is not a non-negative integer.
    pub fn effect(&self, of: &dyn Holder, settlement: FeeSettlement) -> Result<BigInt, IndexError> {
        let mut net = BigInt::ZERO;
        for movement in self
            .movements
            .iter()
            .filter(|movement| &movement.asset == of.asset())
        {
            net += movement.effect(of.address())?;
        }
        let charged = self.fee.as_ref().filter(|fee| {
            settlement == FeeSettlement::Charged
                && fee.payer.as_ref() == Some(of.address())
                && &fee.asset == of.asset()
        });
        if let Some(fee) = charged {
            net -= units(&fee.amount)?;
        }
        Ok(net)
    }
}

impl BlockCredit {
    /// What this transactionless credit does to `of`: its amount when it
    /// names `of`'s address and asset, zero otherwise.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::Store`] when the amount is not a non-negative integer.
    pub fn effect(&self, of: &dyn Holder) -> Result<BigInt, IndexError> {
        if &self.asset == of.asset() && &self.to == of.address() {
            return units(&self.amount);
        }
        Ok(BigInt::ZERO)
    }
}

/// The net of a collection of transactions to one holding.
///
/// A trait only because [`Transactions`] is `base`'s collection, which this
/// crate cannot extend inherently; the computation is this crate's.
pub trait Net {
    /// Every transaction's [`Transaction::effect`] on `of`, summed exactly.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::Store`] when an amount is not a non-negative integer.
    fn net(&self, of: &dyn Holder, settlement: FeeSettlement) -> Result<BigInt, IndexError>;
}

impl Net for Transactions<Transaction> {
    fn net(&self, of: &dyn Holder, settlement: FeeSettlement) -> Result<BigInt, IndexError> {
        self.iter().try_fold(BigInt::ZERO, |net, transaction| {
            Ok(net + transaction.effect(of, settlement)?)
        })
    }
}

/// One stored amount as atomic units.
fn units(amount: &Decimal) -> Result<BigInt, IndexError> {
    amount.to_atomic(0).map(BigInt::from).map_err(|error| {
        IndexError::new(
            IndexErrorKind::Store,
            format!("stored amount is not a non-negative integer: {error}"),
            false,
        )
    })
}
