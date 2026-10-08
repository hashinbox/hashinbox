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
