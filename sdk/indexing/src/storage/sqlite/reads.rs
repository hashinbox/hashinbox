//! The balance and history reads this store answers with inherent methods —
//! not ports: a custom store brings its own. Each read is one snapshot, and
//! reports how far behind its own line the address's inspection is.

use super::models::{Ledger, Transaction, Wallet};
use super::{Repository, release};
use crate::storage::history::charges;
use crate::{Holder, IndexError, Inspection, Page, Position, SettledBalance, WatchProgress};

/// Fewest rows one history query reads while filling a page.
const MIN_CHUNK: usize = 8;

impl Repository {
    /// `of`'s settled balance: the total kept for an observed holding, or
    /// every `ledger` row of it folded in Rust (amounts are u256-capable text,
    /// never summed by SQLite), with how far behind the line the address's
    /// inspection is. Complete it with the pending blocks
    /// ([`SettledBalance::completed`]).
    ///
    /// # Errors
    ///
    /// A `Store` error when a read fails or a row is malformed.
    pub async fn balance(&self, of: &dyn Holder) -> Result<SettledBalance, IndexError> {
        let address = of.address();
        let network = Self::network(&address.network)?;
        let (mut read, checkpoint) = self.database.snapshot(&network).await?;
        let wallets = Wallet::find_observed_by_address(&mut read, &network, &address.value).await?;
        let kept = wallets
            .iter()
            .find(|wallet| wallet.subject().1 == of.asset().asset)
            .map(Wallet::balance)
            .transpose()?
            .flatten();
        let net = match kept {
            Some(net) => net,
            None => {
                let subject = (address.value.as_str(), of.asset().asset.as_str());
                Ledger::net(&mut read, &network, subject).await?
            }
        };
        release(read).await?;
        let observations = wallets
            .into_iter()
            .map(WatchProgress::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let behind = Inspection::new(&observations, checkpoint.as_ref()).behind();
        Ok(SettledBalance::new(net, checkpoint, behind))
    }

    /// Up to `limit` of `of`'s confirmed transactions after `after`, one
    /// snapshot. Complete it with the pending blocks ([`Page::completed`]).
    ///
    /// # Errors
    ///
    /// A `Store` error when a read fails or a row is malformed.
    pub async fn history(
        &self,
        of: &dyn Holder,
        after: Option<&Position>,
        limit: usize,
    ) -> Result<Page, IndexError> {
        let address = of.address();
        let network = Self::network(&address.network)?;
        let (mut read, checkpoint) = self.database.snapshot(&network).await?;
        let observations = Wallet::find_observed_by_address(&mut read, &network, &address.value)
            .await?
            .into_iter()
            .map(WatchProgress::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let behind = Inspection::new(&observations, checkpoint.as_ref()).behind();
        let rows = limit.saturating_add(1);
        let chunk = rows.max(MIN_CHUNK);
        let mut cursor = after.map_or((-1, -1, String::new()), |after| {
            (
                i64::try_from(after.height.0).unwrap_or(i64::MAX),
                i64::try_from(after.order).unwrap_or(i64::MAX),
                after.transaction.id.as_str().to_owned(),
            )
        });
        let mut matches = Vec::new();
        loop {
            let entries = Transaction::find_after_by_address(
                &mut read,
                &network,
                &address.value,
                &cursor,
                chunk,
            )
            .await?
            .into_iter()
            .map(crate::Transaction::try_from)
            .collect::<Result<Vec<_>, _>>()?;
            let exhausted = entries.len() < chunk;
            let (Some(first), Some(last)) = (entries.first(), entries.last()) else {
                break;
            };
            let span = (
                i64::try_from(first.block().height.0).unwrap_or(i64::MAX),
                i64::try_from(last.block().height.0).unwrap_or(i64::MAX),
            );
            let mut movements = Ledger::find_movements(&mut read, &network, of, span).await?;
            for mut transaction in entries {
                cursor = (
                    i64::try_from(transaction.block().height.0).unwrap_or(i64::MAX),
                    i64::try_from(transaction.order).unwrap_or(i64::MAX),
                    transaction.id.id.as_str().to_owned(),
                );
                transaction.movements = movements
                    .remove(transaction.id.id.as_str())
                    .unwrap_or_default();
                let kept = !transaction.movements.is_empty() || charges(of, &transaction);
                matches.extend(kept.then_some(transaction));
            }
            if exhausted || matches.len() >= rows {
                break;
            }
        }
        release(read).await?;
        Ok(Page::new(checkpoint, matches, limit, behind))
    }
}
