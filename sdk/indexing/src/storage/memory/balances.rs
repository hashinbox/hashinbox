//! Watched totals, seeded from retained facts and changed only by novel writes.

use num_bigint::BigInt;

use super::{Repository, Tables};
use crate::{Address, BalanceQuery, CanonicalTransaction, FeeSettlement, Fold, Holder, IndexError};

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
    /// Verifies a watched holding's persisted net against retained facts in one snapshot.
    /// Unwatched holdings have no persisted net to compare. Never repairs rows.
    ///
    /// # Errors
    /// Invalid queries, storage failures, or a terminal store error on disagreement.
    pub async fn verify_balance(&self, query: BalanceQuery) -> Result<(), IndexError> {
        query.validate()?;
        self.read(|tables| {
            let key = (
                query.address.network.clone(),
                query.asset.clone(),
                query.address.value.clone(),
            );
            if let Some(saved) = tables.balances.get(&key)
                && saved != &Self::recomputed(&query, tables)?
            {
                return Err(Self::corrupt(
                    "a persisted balance disagrees with its movements",
                ));
            }
            Ok(())
        })
    }

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
        let holding = (
            credit.to.network.clone(),
            credit.asset.clone(),
            credit.to.value.clone(),
        );
        let Some(net) = tables.balances.get_mut(&holding) else {
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
        transaction: &CanonicalTransaction,
    ) -> Result<(), IndexError> {
        let settlement = tables
            .settlements
            .get(&address.network)
            .copied()
            .unwrap_or(FeeSettlement::Charged);
        for ((network, asset, owner), net) in &mut tables.balances {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AssetId, BalanceStore, BlockPosition, ChainName, Network, Watch, WatchStore};
    use futures_executor::block_on;

    #[test]
    fn saved_balances_are_read_and_verified_independently() {
        let repository = Repository::new();
        let network = Network::new(ChainName::new("chain"), "test");
        let query = BalanceQuery {
            address: Address::new(network.clone(), "a"),
            asset: AssetId::native(network.chain.clone()),
        };
        block_on(repository.add(vec![Watch {
            id: "a".into(),
            address: query.address.clone(),
            asset: query.asset.clone(),
            start: BlockPosition(0),
        }]))
        .expect("watch");
        block_on(repository.verify_balance(query.clone())).expect("empty total agrees");
        // Corrupt the maintained value independently of its source facts.
        repository.tables.lock().expect("tables").balances.insert(
            (network, query.asset.clone(), query.address.value.clone()),
            BigInt::from(5),
        );
        assert_eq!(
            block_on(repository.balance(query.clone()))
                .expect("stored read")
                .amount
                .to_string(),
            "5"
        );
        let error = block_on(repository.verify_balance(query.clone())).expect_err("disagreement");
        assert_eq!(error.kind, crate::IndexErrorKind::Store);
        assert!(!error.retryable);
        assert_eq!(
            block_on(repository.balance(query))
                .expect("verification does not repair")
                .amount
                .to_string(),
            "5"
        );
    }
}
