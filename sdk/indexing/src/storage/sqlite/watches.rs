//! The durable watch list: what to observe, and how far it has been observed.
//!
//! **One table, one row per wallet.** What to observe and how far it has
//! been inspected used to be two tables joined on every read, which bought a
//! foreign key, a cascade and one extra read per recorded claim. They are one
//! row now, so [`WatchStore::all`]'s "one consistent snapshot" is a single
//! statement with nothing to pair — a watch cannot be handed another watch's
//! progress when there is no second row to fetch it from. Reads go through
//! the observed rows only; withdrawing a watch archives its row rather than
//! deleting it (`WalletRegistry`, in `wallets.rs`, is the application's half of
//! the same table).
//!
//! `add` is one `BEGIN IMMEDIATE` transaction for the whole batch, so a wallet
//! registered for its native asset and a token either gets both rows or
//! neither. Uniqueness is still the schema's — `(chain, network, asset,
//! address)` is the primary key and `id` carries a unique index over the file —
//! but a *repeat* has to be recognised rather than refused, so the rows a batch
//! touches are read inside that transaction and compared.

use sqlx::SqliteConnection;

use super::{Repository, commit, found, models};
use crate::storage::watch::redefinition;
use crate::{BoxFuture, IndexError, IndexErrorKind, Network, Watch, WatchProgress, WatchStore};

use models::Wallet;

impl WatchStore for Repository {
    /// One transaction for the batch: every watch is read, compared and
    /// inserted inside it, so a conflicting definition anywhere in the batch
    /// leaves no row behind.
    fn add<'a>(&'a self, watches: Vec<Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            let Some(network) = Self::one_network(&watches)? else {
                return Ok(());
            };
            let rows = watches
                .iter()
                .map(Wallet::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            let mut write = self.database.write().await?;
            Self::register(&mut write, &network, &rows).await?;
            commit(write).await
        })
    }

    /// The watch list and its progress from one snapshot: one statement.
    fn all<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<WatchProgress>, IndexError>> {
        Box::pin(async move {
            let network = Self::network(network)?;
            let mut connection = self.database.connection().await?;
            Wallet::find_observed(&mut connection, &network)
                .await?
                .into_iter()
                .map(WatchProgress::try_from)
                .collect()
        })
    }

    /// One update: the row is archived, and its progress and total go with
    /// it; the row and every indexed fact of its holding stay.
    fn forget<'a>(
        &'a self,
        network: &'a Network,
        watch: &'a Watch,
    ) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            if !watch.address.belongs_to(network) {
                return Err(IndexError::new(
                    IndexErrorKind::NetworkMismatch,
                    "watch belongs to another network",
                    false,
                ));
            }
            let network = Self::network(network)?;
            let mut write = self.database.write().await?;
            Wallet::archive(&mut write, &network, &watch.id).await?;
            commit(write).await
        })
    }

    /// One transaction for the batch, and a **narrowed** write inside it: every
    /// row at the birthday it now carries, with the claim it earned under the
    /// old one gone, and nothing else.
    ///
    /// **Narrowed rather than restricted.** The port's contract is a
    /// postcondition — it states what must be true afterwards, and repeating the
    /// call is how a lost acknowledgement is settled — so refusing a row the
    /// store does not hold would turn that recovery into an unrecoverable state.
    /// What is restricted instead is the *column set*: a re-anchor writes
    /// `birthday` and the recorded claim and nothing else, so the identity the
    /// row is registered under cannot be rewritten by it.
    fn cancel_backfill<'a>(&'a self, watches: Vec<Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            let Some(network) = Self::one_network(&watches)? else {
                return Ok(());
            };
            let rows = watches
                .iter()
                .map(Wallet::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            let mut write = self.database.write().await?;
            Self::reanchor(&mut write, &network, &rows).await?;
            commit(write).await
        })
    }
}

impl Repository {
    /// The one network a batch is about, or `None` for an empty batch.
    ///
    /// A batch is atomic, and this store writes one network's rows per
    /// transaction, so a batch spanning two networks is refused rather than
    /// split into two transactions that could half-succeed.
    fn one_network(watches: &[Watch]) -> Result<Option<Network>, IndexError> {
        let Some(first) = watches.first() else {
            return Ok(None);
        };
        let network = first.address.network.clone();
        for watch in watches {
            watch.validate()?;
            if watch.address.network != network {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidRequest,
                    "a batch of watches must all belong to one network",
                    false,
                ));
            }
        }
        Self::network(&network).map(Some)
    }

    /// Insert-if-absent, verify-if-present, for the whole batch: an observed
    /// row is accepted only as an exact repeat, and an archived row of the
    /// same identity and holding is observed again from the birthday given.
    async fn register(
        transaction: &mut SqliteConnection,
        network: &Network,
        rows: &[Wallet],
    ) -> Result<(), IndexError> {
        for row in rows {
            match Self::taken(transaction, network, row).await? {
                Some(stored) if stored.repeats(row) => {}
                Some(stored) if stored.archived() && stored.holds(row) => {
                    Wallet::reactivate(transaction, row).await?;
                }
                Some(_) => return Err(redefinition()),
                None => Wallet::insert(transaction, row).await?,
            }
        }
        Ok(())
    }

    /// Every row of the batch at the birthday it now carries, with the claim
    /// it earned under the old one gone.
    async fn reanchor(
        transaction: &mut SqliteConnection,
        network: &Network,
        rows: &[Wallet],
    ) -> Result<(), IndexError> {
        for row in rows {
            match Self::taken(transaction, network, row).await? {
                // Found by its pair, or by its identity under another pair:
                // only the first is this row. The second is another subject,
                // which a re-anchor neither moves nor re-dates. An archived
                // row is observed again, as the batch describes it.
                Some(held) if held.holds(row) && held.archived() => {
                    Wallet::reactivate(transaction, row).await?;
                }
                Some(held) if held.holds(row) => Wallet::reanchor(transaction, row, &held).await?,
                Some(_) => return Err(redefinition()),
                None => Wallet::insert(transaction, row).await?,
            }
        }
        Ok(())
    }

    /// The row `row` would replace, addressed by either of the table's two
    /// uniqueness constraints.
    ///
    /// Both are read, because a batch entry can collide on either: the
    /// `(asset, address)` pair it names, or the identity it claims. Archived
    /// rows included, because a registration is compared with every row that
    /// holds its names.
    async fn taken(
        transaction: &mut SqliteConnection,
        network: &Network,
        row: &Wallet,
    ) -> Result<Option<Wallet>, IndexError> {
        let context = "failed to read an observation";
        let held = Wallet::get_by_subject(transaction, network, row.subject()).await;
        if let Some(held) = found(held, context)? {
            return Ok(Some(held));
        }
        found(
            Wallet::get_in_network(transaction, network, row.id()).await,
            context,
        )
    }
}

/// What the shared watch-store contract cannot state, because none of it is a
/// property of the port (`tests/repository_contract.rs` runs that contract
/// against this store and the in-memory one together).
#[cfg(test)]
mod watch_store_test {
    use tempfile::TempDir;

    use super::super::database_test::{network, open};
    use super::*;
    use crate::{Address, AssetId, BlockPosition, ChainName, IndexErrorKind};

    fn watch(network: &Network, id: &str) -> Watch {
        Watch {
            id: id.to_owned(),
            address: Address::new(network.clone(), "a"),
            asset: AssetId::native(network.chain.clone()),
            start: BlockPosition(3),
        }
    }

    /// An observation survives reopening the *file*, and an identity is unique
    /// across every network in it.
    #[tokio::test]
    async fn registrations_survive_reopening_the_file() {
        let directory = TempDir::new().expect("temporary directory");
        let repository = open(&directory, "shared.db").await;
        let elsewhere = Network::new(ChainName::new("other"), "testnet");
        repository
            .add(vec![watch(&network(), "mine")])
            .await
            .expect("mine");
        repository
            .add(vec![watch(&elsewhere, "theirs")])
            .await
            .expect("same address elsewhere");
        let error = repository
            .add(vec![watch(&elsewhere, "mine")])
            .await
            .expect_err("global id");
        assert_eq!(error.kind, IndexErrorKind::Conflict);
        drop(repository);

        let reopened = open(&directory, "shared.db").await;
        let stored = reopened.all(&network()).await.expect("observations");
        assert_eq!(stored, [WatchProgress::unread(watch(&network(), "mine"))]);
    }
}
