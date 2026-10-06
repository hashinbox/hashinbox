//! The application's half of the `wallet` table ([`WalletRegistry`]): filing a
//! wallet with its label and key, and reading every row back, archived ones
//! included. Scanning never reads label or key.

use super::{Repository, commit, found, models};
use crate::storage::watch::redefinition;
use crate::{BoxFuture, IndexError, Registration, Wallet, WalletRegistry};

impl WalletRegistry for Repository {
    /// One transaction: the pair and the identity are read and the row filed
    /// inside it, under the rules of [`WalletRegistry::register`]; the row is
    /// returned as stored.
    fn register<'a>(
        &'a self,
        registration: Registration,
    ) -> BoxFuture<'a, Result<Wallet, IndexError>> {
        Box::pin(async move {
            registration.validate()?;
            let network = Self::network(&registration.address.network)?;
            let row = models::Wallet::from(registration);
            let mut write = self.database.write().await?;
            let held = models::Wallet::get_by_subject(&mut write, &network, row.subject()).await;
            let filed = match found(held, "failed to read an observation")? {
                Some(held) if !held.archived() && held.id() != row.id() => {
                    return Err(redefinition());
                }
                Some(held) => {
                    let filed = held.merged(row)?;
                    models::Wallet::update_registration(&mut write, &filed).await?;
                    filed
                }
                None => {
                    // The identity is unique across the file, whatever network
                    // holds it.
                    let named = models::Wallet::get_by_id(&mut write, row.id()).await;
                    if found(named, "failed to read a wallet")?.is_some() {
                        return Err(redefinition());
                    }
                    models::Wallet::insert(&mut write, &row).await?;
                    row
                }
            };
            commit(write).await?;
            filed.try_into()
        })
    }

    fn wallet<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Wallet>, IndexError>> {
        Box::pin(async move {
            let mut connection = self.database.connection().await?;
            let row = models::Wallet::get_by_id(&mut connection, id).await;
            found(row, "failed to read a wallet")?
                .map(TryInto::try_into)
                .transpose()
        })
    }

    fn wallets<'a>(&'a self) -> BoxFuture<'a, Result<Vec<Wallet>, IndexError>> {
        Box::pin(async move {
            let mut connection = self.database.connection().await?;
            models::Wallet::find_all(&mut connection)
                .await?
                .into_iter()
                .map(TryInto::try_into)
                .collect()
        })
    }
}
