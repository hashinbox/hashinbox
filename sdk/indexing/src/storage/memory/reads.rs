//! The balance and history reads this store answers with inherent methods —
//! not ports: a custom store brings its own. Each read is taken under one
//! lock, and reports how far behind its own line the address's inspection is.

use super::Repository;
use crate::storage::history::selected;
use crate::{Holder, IndexError, Inspection, Page, Position, SettledBalance};

impl Repository {
    /// `of`'s settled balance: the total kept for an observed holding, or its
    /// rows folded exactly, with how far behind the line the address's
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
            let subject = crate::Subject {
                address: address.clone(),
                asset: of.asset().clone(),
            };
            // An unwatched holding has no kept total: its rows are folded.
            let net = match Self::cursor(tables, address, of.asset()) {
                Some(cursor) => cursor.balance.clone(),
                None => Self::recomputed(&subject, tables)?,
            };
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
            let matches = tables
                .history
                .iter()
                .filter(|(key, _)| Self::owns(&key.0, &key.1, address) && past(key))
                .filter_map(|(_, transaction)| selected(of, transaction.clone()))
                // A fee is shown where this address paid it, as SQLite's
                // ledger holds it.
                .map(|mut transaction| {
                    let payer = transaction.fee.as_ref().and_then(|fee| fee.payer.as_ref());
                    let paid = payer == Some(address);
                    transaction.fee = transaction.fee.filter(|_| paid);
                    transaction
                })
                .take(limit.saturating_add(1))
                .collect();
            Ok(Page::new(checkpoint, matches, limit, behind))
        })
    }
}
