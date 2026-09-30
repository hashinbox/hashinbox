//! Persisted watched balances and snapshot-consistent inspection claims.

use num_bigint::BigInt;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::{Repository, Tables, failure, store};
use crate::{
    Balance, BalanceQuery, BalanceStore, BlockRef, BoxFuture, Completeness, Fold, Holder,
    IndexError, Inspection,
};

impl BalanceStore for Repository {
    /// Reads the settled amount, checkpoint and inspection claim in one snapshot.
    /// Watched amounts are persisted; unwatched holdings are folded from effects.
    fn balance<'a>(&'a self, query: BalanceQuery) -> BoxFuture<'a, Result<Balance, IndexError>> {
        Box::pin(async move {
            query.validate()?;
            let tables = self.tables(&query.address.network)?;
            self.database
                .blocking(move |connection| tables.balance(connection, &query))
                .await
        })
    }
}

impl Repository {
    /// Verifies a watched holding's persisted net against its effects in one snapshot.
    /// Unwatched holdings have no persisted net to compare. Never repairs rows.
    ///
    /// # Errors
    /// Invalid queries, storage failures, or a terminal store error on disagreement.
    pub async fn verify_balance(&self, query: BalanceQuery) -> Result<(), IndexError> {
        query.validate()?;
        let tables = self.tables(&query.address.network)?;
        self.database
            .blocking(move |connection| {
                let (read, _) = tables.snapshot(connection)?;
                let subject = (query.address.value.as_str(), query.asset.asset.as_str());
                if let Some(saved) = tables.saved(&read, subject)?
                    && saved != tables.recomputed(&read, subject)?
                {
                    return Err(store("a persisted balance disagrees with its movements"));
                }
                Ok(())
            })
            .await
    }
}

impl Tables {
    /// Reads a watched total in constant space; unwatched holdings are folded.
    pub(super) fn balance(
        &self,
        connection: &Connection,
        query: &BalanceQuery,
    ) -> Result<Balance, IndexError> {
        let (read, checkpoint) = self.snapshot(connection)?;
        let subject = (query.address.value.as_str(), query.asset.asset.as_str());
        let net = match self.saved(&read, subject)? {
            Some(net) => net,
            None => self.recomputed(&read, subject)?,
        };
        let ledger = Fold::from_net(query, net);
        let mut balance = ledger.balance(checkpoint);
        // Provenance only ever *weakens* the answer the arithmetic produced, and
        // never the other way round: `Overdrawn` is a proof the rows carry and a
        // claim about provenance cannot talk it away.
        if balance.completeness != Completeness::Overdrawn {
            balance.completeness = self.provenance(&read, query, balance.observed_at.as_ref())?;
            balance.opening = self.opening(&read, query, balance.observed_at.as_ref())?;
        }
        Ok(balance)
    }

    /// The persisted total, absent only when this holding is not watched.
    pub(super) fn saved(
        &self,
        read: &Transaction<'_>,
        subject: (&str, &str),
    ) -> Result<Option<BigInt>, IndexError> {
        let text: Option<String> = read
            .query_row(
                "SELECT balance FROM watch
                 WHERE chain = ?1 AND network = ?2 AND address = ?3 AND asset = ?4",
                params![self.chain(), self.name(), subject.0, subject.1],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| failure(&error, "failed to read a watched balance"))?;
        text.map(|value| {
            value
                .parse()
                .map_err(|_| store("a stored balance is not an atomic integer"))
        })
        .transpose()
    }

    /// Independent fold used at registration, migration and for unwatched reads.
    pub(super) fn recomputed(
        &self,
        read: &Transaction<'_>,
        subject: (&str, &str),
    ) -> Result<BigInt, IndexError> {
        let (credits, debits) = self.totals(read, subject)?;
        Ok(BigInt::from(credits) - BigInt::from(debits))
    }

    /// Updates only from an effect the insert actually wrote, inside its transaction.
    pub(super) fn accrue(
        &self,
        transaction: &Transaction<'_>,
        subject: (&str, &str),
        delta: BigInt,
    ) -> Result<(), IndexError> {
        if let Some(held) = self.saved(transaction, subject)? {
            let total = held + delta;
            self.save_total(transaction, subject, total)?;
        }
        Ok(())
    }

    /// Seeds every existing holding as part of the schema transaction.
    pub(super) fn seed_balances(transaction: &Transaction<'_>) -> Result<(), IndexError> {
        let mut statement = transaction
            .prepare("SELECT chain, network, address, asset FROM watch")
            .map_err(|error| failure(&error, "failed to prepare balance migration"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
            .map_err(|error| failure(&error, "failed to read balance migration"))?;
        for (chain, network, address, asset) in rows {
            Self::new(crate::Network::new(crate::ChainName::new(chain), network))
                .seed_total(transaction, (&address, &asset))?;
        }
        Ok(())
    }

    pub(super) fn seed_total(
        &self,
        transaction: &Transaction<'_>,
        subject: (&str, &str),
    ) -> Result<(), IndexError> {
        self.save_total(transaction, subject, self.recomputed(transaction, subject)?)
    }

    fn save_total(
        &self,
        transaction: &Transaction<'_>,
        subject: (&str, &str),
        total: BigInt,
    ) -> Result<(), IndexError> {
        transaction
            .execute(
                "UPDATE watch SET balance = ?5
                 WHERE chain = ?1 AND network = ?2 AND address = ?3 AND asset = ?4",
                params![
                    self.chain(),
                    self.name(),
                    subject.0,
                    subject.1,
                    total.to_string()
                ],
            )
            .map(drop)
            .map_err(|error| failure(&error, "failed to write a watched balance"))
    }

    /// What the observations of `of`'s address can claim about the history
    /// behind its rows, at the `checkpoint` of the snapshot `read` holds.
    ///
    /// **Answered once for both reads.** A balance and a history page are two
    /// projections of one address's stored rows, so what the store can prove
    /// about the range behind them is one fact, read here inside whichever
    /// read's own snapshot asked for it (`Tables::page` is the other caller).
    /// Two copies of this rule could disagree, and a caller comparing a
    /// complete-looking page against an incomplete-looking total would have no
    /// way to tell which of them was lying.
    ///
    /// **Computed, never stored.** A whole-history claim is a conjunction of
    /// facts the `watch` rows already carry and the line this snapshot was read
    /// at: observation began at the chain's first block, progress has been
    /// recorded for it, **and** no observation of the address is still behind
    /// that line. The rule is [`Inspection`]'s and both stores ask it; what is
    /// this store's own is that the rows it is asked over come from the same
    /// transaction as the answer they qualify, which is why no index has to
    /// repair the claim afterwards.
    pub(super) fn provenance(
        &self,
        read: &Transaction<'_>,
        of: &dyn Holder,
        checkpoint: Option<&BlockRef>,
    ) -> Result<Completeness, IndexError> {
        let observations = self.observing(read, &of.address().value)?;
        Ok(Inspection::new(&observations, checkpoint).completeness(of))
    }
}
