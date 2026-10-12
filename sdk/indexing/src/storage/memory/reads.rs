//! The balance and history reads this store answers with inherent methods —
//! not ports: a custom store brings its own. Each read is taken under one
//! lock, and reports how far behind its own line the address's inspection is.

use super::Repository;
use crate::storage::LedgerEntry;
use crate::storage::history::charges;
use crate::{Holder, IndexError, Inspection, Page, Position, SettledBalance};

impl Repository {
    /// `of`'s settled balance: its holding's last running total, watched or
    /// not, with how far behind the line the address's
    /// inspection is. Complete it with the pending blocks
    /// ([`SettledBalance::completed`]).
    ///
    /// # Errors
    ///
    /// A `Store` error for a poisoned lock or a malformed amount.
    pub fn balance(&self, of: &dyn Holder) -> Result<SettledBalance, IndexError> {
        let address = of.address();
        self.read(|tables| {
            let checkpoint = tables.checkpoints.get(&address.network).cloned();
            let net = tables
                .ledger
                .get(&(address.clone(), of.asset().clone()))
                .and_then(|movements| movements.last())
                .map_or_else(num_bigint::BigInt::default, |last| last.total.clone());
            let observations = Self::observing(tables, address);
            let behind = Inspection::new(&observations, checkpoint.as_ref()).behind();
            Ok(SettledBalance::new(net, checkpoint, behind))
        })
    }

    /// Up to `limit` of `of`'s confirmed transactions after `after`, under one
    /// lock. Complete it with the pending blocks ([`Page::completed`]).
    ///
    /// # Errors
    ///
    /// A `Store` error for a poisoned lock.
    pub fn history(
        &self,
        of: &dyn Holder,
        after: Option<&Position>,
        limit: usize,
    ) -> Result<Page, IndexError> {
        let address = of.address();
        self.read(|tables| {
            let checkpoint = tables.checkpoints.get(&address.network).cloned();
            let observations = Self::observing(tables, address);
            let behind = Inspection::new(&observations, checkpoint.as_ref()).behind();
            let past = |(_, _, height, order, id): &super::HistoryPlace| {
                after.is_none_or(|after| {
                    (height.0, *order, id.id.as_str())
                        > (after.height.0, after.order, after.transaction.id.as_str())
                })
            };
            // Movements and fee from the address's entries, exactly as
            // SQLite reads its ledger rows ([`LedgerEntry::history`]).
            let entries = tables
                .ledger
                .iter()
                .filter(|((held, _), _)| held == address)
                .flat_map(|(_, movements)| movements.iter().map(|movement| &movement.entry));
            let mut effects = LedgerEntry::history(entries, of)?;
            let mut matches = Vec::new();
            let rows = tables
                .history
                .iter()
                .filter(|(key, _)| Self::owns(&key.0, &key.1, address) && past(key));
            for (_, transaction) in rows {
                let mut transaction = transaction.clone();
                (transaction.movements, transaction.fee) = effects
                    .remove(transaction.id.id.as_str())
                    .unwrap_or_default();
                let kept = !transaction.movements.is_empty() || charges(of, &transaction);
                matches.extend(kept.then_some(transaction));
                if matches.len() > limit {
                    break;
                }
            }
            Ok(Page::new(checkpoint, matches, limit, behind))
        })
    }
}
