//! The in-memory store's wallet registry ([`WalletRegistry`]): filing a wallet
//! with its label and key, and reading every row back, archived ones included.
//! The rules are SQLite's, and the shared contract (`tests/contracts/wallets.rs`)
//! holds both stores to them.

use std::sync::Arc;

use crypto::SecretBytes;

use super::{Repository, Tables, WalletRow, observations::WalletKey};
use crate::storage::{wallet, watch::redefinition};
use crate::{Address, BoxFuture, IndexError, Registration, Wallet, WalletRegistry};

impl WalletRegistry for Repository {
    fn register<'a>(
        &'a self,
        registration: Registration,
    ) -> BoxFuture<'a, Result<Wallet, IndexError>> {
        Box::pin(async move {
            registration.validate()?;
            let (wallet_key, row) = self.apply(|tables| Self::file(tables, registration))?;
            Self::wallet_of(&wallet_key, &row)
        })
    }

    fn wallet<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<Wallet>, IndexError>> {
        Box::pin(async move {
            self.read(|tables| Ok(Self::located(tables, id)))?
                .map(|(wallet_key, row)| Self::wallet_of(&wallet_key, &row))
                .transpose()
        })
    }

    fn wallets<'a>(&'a self) -> BoxFuture<'a, Result<Vec<Wallet>, IndexError>> {
        Box::pin(async move {
            let mut rows = self.read(|tables| {
                Ok(tables
                    .wallets
                    .iter()
                    .map(|(wallet_key, row)| (wallet_key.clone(), row.clone()))
                    .collect::<Vec<_>>())
            })?;
            rows.sort_by(|(_, left), (_, right)| left.id.cmp(&right.id));
            rows.iter()
                .map(|(wallet_key, row)| Self::wallet_of(wallet_key, row))
                .collect()
        })
    }
}

impl Repository {
    /// Files `registration` under the rules of [`WalletRegistry::register`],
    /// and returns the row as stored.
    fn file(
        tables: &mut Tables,
        registration: Registration,
    ) -> Result<(WalletKey, WalletRow), IndexError> {
        let wallet_key = (
            registration.address.network.clone(),
            registration.asset.clone(),
            registration.address.value.clone(),
        );
        let secret = registration.secret.map(Arc::new);
        let Some(row) = tables.wallets.get_mut(&wallet_key) else {
            if tables.identities.contains_key(&registration.id) {
                return Err(redefinition());
            }
            let row = WalletRow {
                id: registration.id,
                birthday: None,
                label: registration.label,
                secret,
                archived_at: Some(wallet::now()),
            };
            tables
                .identities
                .insert(row.id.clone(), wallet_key.0.clone());
            tables.wallets.insert(wallet_key.clone(), row.clone());
            return Ok((wallet_key, row));
        };
        if row.archived_at.is_none() && row.id != registration.id {
            return Err(redefinition());
        }
        match (&row.secret, secret) {
            (Some(held), Some(given)) if held.as_bytes() != given.as_bytes() => {
                return Err(wallet::another_key());
            }
            (None, given) => row.secret = given,
            _ => {}
        }
        if registration.label.is_some() {
            row.label = registration.label;
        }
        Ok((wallet_key, row.clone()))
    }

    /// The public wallet one row is, with its own copy of the key.
    fn wallet_of(
        (network, asset, address): &WalletKey,
        row: &WalletRow,
    ) -> Result<Wallet, IndexError> {
        let registration = Registration {
            id: row.id.clone(),
            address: Address::new(network.clone(), address.clone()),
            asset: asset.clone(),
            label: row.label.clone(),
            secret: row
                .secret
                .as_deref()
                .map(|secret| SecretBytes::new(secret.as_bytes())),
        };
        Wallet::new(registration, row.birthday, row.archived_at)
    }
}
