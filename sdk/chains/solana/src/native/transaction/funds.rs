use std::collections::BTreeMap;

use crate::native::{Address, Error, ErrorKind, Lamport};

/// What each sender of a batch must afford: every transfer it sends plus
/// every fee it pays, against one observed balance. Incoming transfers in
/// the same batch are never credited.
#[derive(Debug, Default)]
pub struct Funds {
    available: BTreeMap<Address, Lamport>,
    required: BTreeMap<Address, Lamport>,
}

impl Funds {
    /// The observed balance left after earlier batch items and this fee.
    /// Incoming transfers in the batch are deliberately not funding.
    #[must_use]
    pub fn remaining(&self, from: &Address, balance: Lamport, fee: Lamport) -> Option<Lamport> {
        balance
            .checked_sub(self.required.get(from).copied().unwrap_or(Lamport::ZERO))?
            .checked_sub(fee)
    }

    /// Adds one transfer of `amount` plus `fee` from `from`, whose observed
    /// balance is `balance`, and checks the sender can still pay everything
    /// added so far.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::RpcUnavailable`] when `balance` differs from the one
    /// observed for an earlier transfer of `from`,
    /// [`ErrorKind::InvalidTransaction`] on overflow, and
    /// [`ErrorKind::InsufficientFunds`] once the total exceeds the balance.
    pub fn require(
        &mut self,
        from: &Address,
        balance: Lamport,
        amount: Lamport,
        fee: Lamport,
    ) -> Result<(), Error> {
        let inconsistent = self
            .available
            .insert(from.clone(), balance)
            .is_some_and(|previous| previous != balance);
        if inconsistent {
            return Err(Error::rpc(
                "Solana source balance is inconsistent between transfers",
                true,
            ));
        }
        let overflow = || {
            Error::new(
                ErrorKind::InvalidTransaction,
                "Solana source requirement overflowed",
            )
        };
        let total = amount
            .checked_add(fee)
            .and_then(|needed| {
                self.required
                    .get(from)
                    .copied()
                    .unwrap_or(Lamport::ZERO)
                    .checked_add(needed)
            })
            .ok_or_else(overflow)?;
        self.required.insert(from.clone(), total);
        if total > balance {
            return Err(Error::new(
                ErrorKind::InsufficientFunds,
                "Solana source balance is insufficient",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lamports(value: u64) -> Lamport {
        Lamport::from_atomic(value)
    }

    #[test]
    fn an_inconsistent_observed_balance_precedes_amount_validation() {
        let source = Address::from_bytes([1; 32]);
        let mut funds = Funds::default();
        funds
            .require(&source, lamports(10), lamports(1), lamports(1))
            .unwrap();
        let error = funds
            .require(&source, lamports(11), lamports(u64::MAX), lamports(1))
            .unwrap_err();
        assert_eq!(
            (error.kind, error.message.as_str()),
            (
                ErrorKind::RpcUnavailable,
                "Solana source balance is inconsistent between transfers"
            )
        );
    }

    #[test]
    fn cumulative_sufficiency_never_credits_incoming_transfers() {
        let source = Address::from_bytes([1; 32]);
        let mut funds = Funds::default();
        funds
            .require(&source, lamports(10), lamports(4), lamports(1))
            .unwrap();
        let error = funds
            .require(&source, lamports(10), lamports(5), lamports(1))
            .expect_err("second occurrence crosses the threshold");
        assert_eq!(
            (error.kind, error.message.as_str()),
            (
                ErrorKind::InsufficientFunds,
                "Solana source balance is insufficient"
            )
        );

        let mut exact = Funds::default();
        exact
            .require(&source, lamports(11), lamports(4), lamports(1))
            .unwrap();
        exact
            .require(&source, lamports(11), lamports(5), lamports(1))
            .expect("exact balance");
    }

    #[test]
    fn requirement_overflow_is_an_invalid_transaction() {
        let source = Address::from_bytes([1; 32]);
        let error = Funds::default()
            .require(&source, lamports(u64::MAX), lamports(u64::MAX), lamports(1))
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidTransaction);
    }
}
