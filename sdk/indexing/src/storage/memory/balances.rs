//! Watched totals, seeded from retained facts and changed only by novel writes.

use num_bigint::BigInt;

use super::{Repository, Tables};
use crate::{Address, BalanceQuery, FeeSettlement, Fold, IndexError, Transaction};

impl Tables {
    /// A rescan preserves registrations but clears every claim about removed facts.
    pub(super) fn disclaim(&mut self, network: &crate::Network) {
        self.progress.retain(|(bound, ..), _| bound != network);
        self.settlements.remove(network);
        for (_, net) in self
            .balances
            .iter_mut()
            .filter(|((bound, ..), _)| bound == network)
        {
            *net = BigInt::from(0);
        }
    }
}

impl Repository {
    pub(super) fn recomputed(query: &BalanceQuery, tables: &Tables) -> Result<BigInt, IndexError> {
        let mut ledger = Fold::new(query);
        let settlement = tables
            .settlements
            .get(&query.address.network)
            .copied()
            .unwrap_or(FeeSettlement::Charged);
        let rows = tables
            .history
            .iter()
            .filter(|((network, address, ..), _)| Self::owns(network, address, &query.address));
        for (_, transaction) in rows {
            ledger.apply(transaction, settlement)?;
        }
        let credited = tables
            .credits
            .iter()
            .filter(|((network, address, ..), _)| Self::owns(network, address, &query.address));
        for (_, (_, credit)) in credited {
            ledger.apply_credit(credit)?;
        }
        Ok(ledger.net())
    }

    pub(super) fn accrue_credit(
        tables: &mut Tables,
        credit: &crate::BlockCredit,
    ) -> Result<(), IndexError> {
        let key = (
            credit.to.network.clone(),
            credit.asset.clone(),
            credit.to.value.clone(),
        );
        let Some(net) = tables.balances.get_mut(&key) else {
            return Ok(());
        };
        let units = credit
            .amount
            .to_atomic(0)
            .map_err(|_| Self::corrupt("a credit is not an atomic integer"))?;
        *net += BigInt::from(units);
        Ok(())
    }

    pub(super) fn accrue_transaction(
        tables: &mut Tables,
        address: &Address,
        transaction: &Transaction,
    ) -> Result<(), IndexError> {
        let settlement = tables
            .settlements
            .get(&address.network)
            .copied()
            .unwrap_or(FeeSettlement::Charged);
        for (key, net) in &mut tables.balances {
            let (network, asset, owner) = key;
            if network != &address.network || owner != &address.value {
                continue;
            }
            let query = BalanceQuery {
                address: address.clone(),
                asset: asset.clone(),
            };
            let mut fold = Fold::new(&query);
            fold.apply(transaction, settlement)?;
            *net += fold.net();
        }
        Ok(())
    }
}
