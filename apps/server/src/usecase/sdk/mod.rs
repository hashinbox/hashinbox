//! `base`'s store traits (`BlockStore`, `WalletIndexStore`,
//! `TransactionIndexStore`, `InputStore`, `WalletStore`, `Holdings`) on the
//! models, one file per trait, all on [`Stores`]. A store only orchestrates;
//! every statement is a model's.

mod blocks;
mod holdings;
mod indexes;
mod inputs;
pub(crate) mod transaction_indexes;
mod wallets;

use std::sync::Arc;

use crate::{db::Db, models::market_candle_1h::MarketCandles1hExchangeAssetMapCached};

/// Every `base` store over the one SQLite file, the legs it keeps valued
/// by `candles` (the market job's, in memory). Clones share the pool.
#[derive(Clone)]
pub(crate) struct Stores {
    db: Db,
    candles: MarketCandles1hExchangeAssetMapCached,
}

impl Stores {
    /// Every store on `db`, valuing by `candles`.
    pub(crate) const fn new(db: Db, candles: MarketCandles1hExchangeAssetMapCached) -> Self {
        Self { db, candles }
    }

    /// Every store on `db`, valuing by `candles`, as the services take
    /// them.
    pub(crate) fn storage(db: Db, candles: MarketCandles1hExchangeAssetMapCached) -> base::Storage {
        let stores = Arc::new(Self::new(db, candles));
        base::Storage {
            blocks: stores.clone(),
            indexes: stores.clone(),
            transaction_indexes: stores.clone(),
            inputs: stores.clone(),
            wallets: stores.clone(),
            holdings: stores,
        }
    }
}
