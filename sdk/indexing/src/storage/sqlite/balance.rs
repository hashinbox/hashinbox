//! Persisted watched balances and snapshot-consistent inspection claims.

use super::{Repository, found, models, release};
use crate::{Balance, BalanceQuery, BalanceStore, BoxFuture, Completeness, Fold, IndexError};

use models::{Ledger, Wallet};

impl BalanceStore for Repository {
    /// Reads the settled amount, checkpoint and inspection claim in one snapshot.
    /// Watched amounts are persisted; unwatched holdings are folded from effects.
    fn balance<'a>(&'a self, query: BalanceQuery) -> BoxFuture<'a, Result<Balance, IndexError>> {
        Box::pin(async move {
            query.validate()?;
            let network = Self::network(&query.address.network)?;
            let (mut read, checkpoint) = self.database.snapshot(&network).await?;
            let subject = (query.address.value.as_str(), query.asset.asset.as_str());
            let observed = Wallet::get_observed_by_subject(&mut read, &network, subject).await;
            let saved = found(observed, "failed to read a watched balance")?;
            let net = match saved.map(|row| row.balance()).transpose()?.flatten() {
                Some(net) => net,
                None => Ledger::net(&mut read, &network, subject).await?,
            };
            let mut balance = Fold::from_net(&query, net).balance(checkpoint);
            // Provenance only ever *weakens* the answer the arithmetic produced,
            // and never the other way round: `Overdrawn` is a proof the rows
            // carry and a claim about provenance cannot talk it away.
            if balance.completeness != Completeness::Overdrawn {
                let at = balance.observed_at.as_ref();
                balance.completeness =
                    Wallet::completeness(&mut read, &network, &query, at).await?;
            }
            release(read).await?;
            Ok(balance)
        })
    }
}
