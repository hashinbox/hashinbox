//! `base`'s store traits (`BlockStore`, `WalletIndexStore`, `InputStore`,
//! `WalletStore`, `Holdings`) on the models, one file per trait, all on
//! [`Stores`]. A store only orchestrates; every statement is a model's.

mod blocks;
mod holdings;
mod indexes;
mod inputs;
mod wallets;

use std::sync::Arc;

use crate::db::Db;

/// Every `base` store over the one SQLite file. Clones share the pool.
#[derive(Clone)]
pub(crate) struct Stores {
    db: Db,
}

impl Stores {
    /// Every store on `db`.
    pub(crate) const fn new(db: Db) -> Self {
        Self { db }
    }

    /// Every store on `db`, as the services take them.
    pub(crate) fn storage(db: Db) -> base::Storage {
        let stores = Arc::new(Self::new(db));
        base::Storage {
            blocks: stores.clone(),
            indexes: stores.clone(),
            inputs: stores.clone(),
            wallets: stores.clone(),
            holdings: stores,
        }
    }
}
