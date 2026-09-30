use std::collections::BTreeMap;

use super::{BuildContext, Transfer};
use crate::native::{
    Address, AssetKind, Error, ErrorKind, Wei,
    rpc::{Accounts, View},
};

/// What each sender of a batch must afford, per asset: every transfer it
/// sends plus the worst-case fee of each, in ether. Incoming transfers in
/// the same batch are never credited.
#[derive(Debug, Default)]
pub struct Funds {
    required: BTreeMap<(Address, AssetKind), Requirement>,
}

/// One sender's total of one asset, and which items contribute to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requirement {
    /// The paying sender.
    pub from: Address,
    /// The asset it pays.
    pub asset: AssetKind,
    /// The sum of every contribution.
    pub amount: Wei,
    contributions: Vec<(usize, Wei)>,
}

impl Funds {
    /// Adds batch item `index`: `transfer` signed under `context`, which
    /// costs its sender at most `gas_limit * max_fee_per_gas` ether.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::FeeUnavailable`] when the fee overflows and
    /// [`ErrorKind::InsufficientFunds`] when a total overflows.
    pub fn require(
        &mut self,
        index: usize,
        transfer: &Transfer,
        context: &BuildContext,
    ) -> Result<(), Error> {
        let fee = context
            .max_fee_per_gas
            .checked_mul_u64(context.gas_limit)
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::FeeUnavailable,
                    "Ethereum aggregate maximum fee overflows U256",
                )
            })?;
        let native = fee.checked_add(&transfer.value()).ok_or_else(|| {
            Error::new(
                ErrorKind::InsufficientFunds,
                "Ethereum aggregate native requirement overflows U256",
            )
        })?;
        self.add(index, &context.from, AssetKind::Native, native)?;
        if let AssetKind::Erc20(_) = transfer.asset {
            self.add(
                index,
                &context.from,
                transfer.asset.clone(),
                transfer.amount.clone(),
            )?;
        }
        Ok(())
    }

    /// The observed balance left after this source's earlier batch items,
    /// including native maximum fees. Incoming transfers never contribute.
    #[must_use]
    pub fn remaining(&self, from: &Address, asset: &AssetKind, balance: &Wei) -> Option<Wei> {
        let prior = self.required.get(&(from.clone(), asset.clone()));
        balance.checked_sub(&prior.map_or(Wei::ZERO, |value| value.amount.clone()))
    }

    /// Every sender's requirement, ordered by its first contributing item.
    #[must_use]
    pub fn requirements(self) -> Vec<Requirement> {
        let mut values = self.required.into_values().collect::<Vec<_>>();
        values.sort_by_key(Requirement::first_index);
        values
    }

    /// Reads each requirement's balance, earliest first, and names the
    /// earliest item whose cumulative requirement crosses its balance.
    ///
    /// # Errors
    ///
    /// The failing item's index with [`ErrorKind::InsufficientFunds`], or
    /// with the balance read's error.
    pub async fn afford(self, accounts: &dyn Accounts) -> Result<(), (usize, Error)> {
        let mut insufficient: Option<usize> = None;
        for requirement in self.requirements() {
            let first = requirement.first_index();
            if insufficient.is_some_and(|index| first >= index) {
                break;
            }
            let balance = accounts
                .balance(requirement.from.clone(), &requirement.asset, View::Pending)
                .await
                .map_err(|error| (first, error))?;
            if balance < requirement.amount {
                let index = requirement.failure_index(&balance);
                insufficient = Some(insufficient.map_or(index, |current| current.min(index)));
            }
        }
        match insufficient {
            Some(index) => Err((
                index,
                Error::new(
                    ErrorKind::InsufficientFunds,
                    "Ethereum aggregate batch balance is insufficient",
                ),
            )),
            None => Ok(()),
        }
    }

    fn add(
        &mut self,
        index: usize,
        from: &Address,
        asset: AssetKind,
        amount: Wei,
    ) -> Result<(), Error> {
        let requirement = self
            .required
            .entry((from.clone(), asset.clone()))
            .or_insert_with(|| Requirement {
                from: from.clone(),
                asset,
                amount: Wei::ZERO,
                contributions: Vec::new(),
            });
        requirement.amount = requirement.amount.checked_add(&amount).ok_or_else(|| {
            Error::new(
                ErrorKind::InsufficientFunds,
                "Ethereum aggregate asset requirement overflows U256",
            )
        })?;
        requirement.contributions.push((index, amount));
        Ok(())
    }
}

impl Requirement {
    /// The first batch item that contributes to this requirement.
    #[must_use]
    pub fn first_index(&self) -> usize {
        self.contributions.first().map_or(0, |(index, _)| *index)
    }

    /// The first item at which the running total exceeds `balance`.
    #[must_use]
    pub fn failure_index(&self, balance: &Wei) -> usize {
        let mut cumulative = Wei::ZERO;
        for (index, amount) in &self.contributions {
            let Some(next) = cumulative.checked_add(amount) else {
                return *index;
            };
            cumulative = next;
            if &cumulative > balance {
                return *index;
            }
        }
        self.first_index()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAIN_ID: u64 = 31_337;

    fn native(value: u128) -> Transfer {
        Transfer {
            asset: AssetKind::Native,
            to: Address::from_bytes([0x55; 20]),
            amount: Wei::from_u128(value),
        }
    }

    fn token(token: &Address, amount: Wei) -> Transfer {
        Transfer {
            asset: AssetKind::Erc20(token.clone()),
            to: Address::from_bytes([0x55; 20]),
            amount,
        }
    }

    fn context(from: &Address, fee: Wei, gas_limit: u64) -> BuildContext {
        BuildContext {
            chain_id: CHAIN_ID,
            from: from.clone(),
            nonce: 0,
            gas_limit,
            max_fee_per_gas: fee,
            max_priority_fee_per_gas: Wei::ZERO,
        }
    }

    fn funds(items: &[(Transfer, BuildContext)]) -> Result<Funds, (usize, Error)> {
        let mut funds = Funds::default();
        for (index, (transfer, context)) in items.iter().enumerate() {
            funds
                .require(index, transfer, context)
                .map_err(|error| (index, error))?;
        }
        Ok(funds)
    }

    #[test]
    fn aggregate_requirements_preserve_native_token_contributions_and_failure_indices() {
        let source = Address::from_bytes([1; 20]);
        let asset = Address::from_bytes([2; 20]);
        let requirements = funds(&[
            (native(7), context(&source, Wei::from_u128(2), 3)),
            (
                token(&asset, Wei::from_u128(11)),
                context(&source, Wei::from_u128(3), 2),
            ),
            (native(5), context(&source, Wei::from_u128(2), 1)),
        ])
        .unwrap()
        .requirements();
        assert_eq!(requirements.len(), 2);
        let native = &requirements[0];
        assert_eq!(native.from, source);
        assert_eq!(native.asset, AssetKind::Native);
        assert_eq!(native.amount, Wei::from_u128(26));
        assert_eq!(native.first_index(), 0);
        assert_eq!(native.failure_index(&Wei::from_u128(12)), 0);
        assert_eq!(native.failure_index(&Wei::from_u128(18)), 1);
        assert_eq!(native.failure_index(&Wei::from_u128(25)), 2);
        let erc20 = &requirements[1];
        assert_eq!(erc20.from, source);
        assert_eq!(erc20.asset, AssetKind::Erc20(asset));
        assert_eq!(erc20.amount, Wei::from_u128(11));
        assert_eq!(erc20.first_index(), 1);
    }

    #[test]
    fn aggregate_overflow_keeps_the_original_item_and_exact_error_class() {
        let source = Address::from_bytes([1; 20]);
        let asset = Address::from_bytes([2; 20]);
        let max = Wei::from_atomic([255; 32]);
        for (items, kind, message) in [
            (
                vec![
                    (native(0), context(&source, Wei::ZERO, 1)),
                    (native(0), context(&source, max.clone(), 2)),
                ],
                ErrorKind::FeeUnavailable,
                "Ethereum aggregate maximum fee overflows U256",
            ),
            (
                vec![
                    (native(0), context(&source, Wei::ZERO, 1)),
                    (native(1), context(&source, max.clone(), 1)),
                ],
                ErrorKind::InsufficientFunds,
                "Ethereum aggregate native requirement overflows U256",
            ),
            (
                vec![
                    (native(0), context(&source, max.clone(), 1)),
                    (native(1), context(&source, Wei::ZERO, 1)),
                ],
                ErrorKind::InsufficientFunds,
                "Ethereum aggregate asset requirement overflows U256",
            ),
            (
                vec![
                    (token(&asset, max.clone()), context(&source, Wei::ZERO, 1)),
                    (
                        token(&asset, Wei::from_u128(1)),
                        context(&source, Wei::ZERO, 1),
                    ),
                ],
                ErrorKind::InsufficientFunds,
                "Ethereum aggregate asset requirement overflows U256",
            ),
        ] {
            let (index, error) = funds(&items).expect_err("sum must overflow");
            assert_eq!(index, 1);
            assert_eq!(error.kind, kind);
            assert_eq!(error.message, message);
        }
    }
}
