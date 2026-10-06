//! Confirmed transaction pages from `transaction_address` rows and shared transaction facts.
//! Native transaction order makes the cursor independent of which addresses were
//! selected when facts first arrived; the snapshot also carries watch coverage.

use sqlx::SqliteConnection;

use super::{Repository, models, release};
use crate::{
    BoxFuture, ConfirmedHistory, ConfirmedHistoryPage, HistoryQuery, Holder, IndexError, Network,
    Transaction, storage::Probe,
};

use models::{HistoryEntry, Ledger, TransactionAddress, Wallet};

/// Fewest rows one query reads while filling a page: a page of a busy address
/// in a rare asset reads many rows, so a small `limit` must not mean one query
/// per row.
const MIN_CHUNK: usize = 8;

impl ConfirmedHistory for Repository {
    /// Reads the checkpoint and the page inside one deferred transaction, so
    /// the page is one snapshot rather than several reads that agreed
    /// afterwards.
    ///
    /// The page's provenance is read inside the same snapshot as its rows, and
    /// by the same function the balance read asks
    /// ([`Wallet::completeness`]): what the store can prove about the range
    /// behind an address's rows is one fact, so the two reads cannot answer it
    /// differently.
    fn history<'a>(
        &'a self,
        request: HistoryQuery,
    ) -> BoxFuture<'a, Result<ConfirmedHistoryPage, IndexError>> {
        Box::pin(async move {
            request.validate()?;
            let network = Self::network(&request.address.network)?;
            let (mut read, checkpoint) = self.database.snapshot(&network).await?;
            let completeness =
                Wallet::completeness(&mut read, &network, &request, checkpoint.as_ref()).await?;
            let probe = Probe::new(request.limit);
            let matches = Self::matching(&mut read, &network, &request, &probe).await?;
            release(read).await?;
            let (transactions, next) = probe.page(matches, |last| last.into());
            Ok(ConfirmedHistoryPage {
                checkpoint,
                transactions,
                next,
                completeness,
            })
        })
    }
}

impl Repository {
    /// The transactions after the position that match `request`, in history
    /// order and at least as many as `probe` asks for when that many exist,
    /// each as [`HistoryQuery::select`] returns it.
    ///
    /// Each chunk's movements in the request's asset are attached
    /// ([`Self::attach`]). The asset narrows
    /// what is attached without narrowing what is read: a `transaction_address` row counts
    /// for a holding even when none of its movements are in that asset,
    /// because the transaction may still carry a fee the holding paid. A
    /// movement in another asset is never one [`HistoryQuery::select`] keeps,
    /// so it is not read at all, and the narrowing is what lets the read walk
    /// `ledger`'s key within the asset rather than every asset of the address.
    async fn matching(
        read: &mut SqliteConnection,
        network: &Network,
        request: &HistoryQuery,
        probe: &Probe,
    ) -> Result<Vec<Transaction>, IndexError> {
        // Rows are read in native block order, so the cursor never depends on
        // the subset of transactions selected for one address.
        let address = request.address().value.as_str();
        let mut after = match &request.after {
            Some(cursor) => TransactionAddress::after(address, cursor)?,
            None => TransactionAddress::first(network, address),
        };
        let chunk = probe.rows().max(MIN_CHUNK);
        let mut matches = Vec::new();
        loop {
            let entries = HistoryEntry::find_after(read, network, address, &after, chunk)
                .await?
                .into_iter()
                .map(<(TransactionAddress, Transaction)>::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            let exhausted = entries.len() < chunk;
            let subject = (address, request.asset.asset.as_str());
            for (place, entry) in Self::attach(read, network, subject, entries).await? {
                after = place;
                matches.extend(request.select(entry));
            }
            if exhausted || probe.answered(matches.len()) {
                return Ok(matches);
            }
        }
    }

    /// Fetches every movement `holding` has in `entries` in one statement,
    /// bounded by the first and last entry the chunk holds, and gives each
    /// entry its own.
    async fn attach(
        read: &mut SqliteConnection,
        network: &Network,
        subject: (&str, &str),
        mut entries: Vec<(TransactionAddress, Transaction)>,
    ) -> Result<Vec<(TransactionAddress, Transaction)>, IndexError> {
        let (Some((lower, _)), Some((upper, _))) = (entries.first(), entries.last()) else {
            return Ok(entries);
        };
        let rows = Ledger::find_attached(read, network, subject, (lower, upper)).await?;
        let mut held = Ledger::group_movements(rows)?;
        for (place, entry) in &mut entries {
            entry.movements = held.remove(&place.transaction_id).unwrap_or_default();
        }
        Ok(entries)
    }
}
