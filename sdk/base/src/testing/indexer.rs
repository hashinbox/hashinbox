//! A real [`Indexer`] over doubles, and stepping it until idle.

use std::{sync::Arc, time::Duration};

use crate::{
    Balances, BlockStore, Chained, Confirmer, Indexer, Next, TransactionIndexStore,
    TransactionIndexer, WalletIndexStore, WalletIndexer,
};

/// An indexer over `chain` into `stores`, both kinds of index, at interval
/// zero: its wallet indexes settle with `balances` (none: they jump), its
/// transaction indexes with `confirmer` (none: they are given up).
pub fn indexer<S>(
    chain: impl Chained + 'static,
    stores: Arc<S>,
    balances: Option<Arc<dyn Balances>>,
    confirmer: Option<Arc<dyn Confirmer>>,
) -> Indexer
where
    S: BlockStore + WalletIndexStore + TransactionIndexStore + 'static,
{
    let chain: Arc<dyn Chained> = Arc::new(chain);
    let wallets = WalletIndexer::new(stores.clone(), chain.clone(), balances);
    let transactions = TransactionIndexer::new(stores.clone(), chain.clone(), confirmer);
    let parts: Vec<Arc<dyn crate::Indexes>> = vec![Arc::new(wallets), Arc::new(transactions)];
    Indexer::new(chain, stores, parts, Duration::ZERO)
}

/// Steps `indexer` until it is idle.
///
/// # Panics
/// When a step fails.
pub async fn idle(indexer: &Indexer) {
    while indexer.next().await.expect("a step") != Next::Idle {}
}
