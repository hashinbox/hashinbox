//! Every wallet this server knows (keyed or watch-only, with its tags and how
//! far each of its indexes is indexed: the `wallet` model's one query) with
//! each index's balance (`balance`). One snapshot.

use base::Decimal;

use crate::{
    db::Db,
    models::{balance, wallet, wallet::Indexed, wallet_index::WalletIndex},
};

/// One wallet and the balance of each of its indexes.
pub(crate) struct Entry {
    /// The wallet, its tags, and how far each of its indexes is indexed.
    pub(crate) wallet: Indexed,
    /// Each index's balance over every live block, in index order.
    pub(crate) balances: Vec<Option<Decimal>>,
}

/// Every wallet in id order.
///
/// # Errors
///
/// The read, or a stored value that does not decode.
pub(crate) async fn list(db: &Db) -> anyhow::Result<Vec<Entry>> {
    db.read(move |connection| {
        let wallets = wallet::db::find_with_indexes(connection, None)?;
        let ids = wallets
            .iter()
            .flat_map(|one| one.indexes.iter())
            .map(|index| WalletIndex::id(&index.address, &index.asset))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        // Confirmed through every position: every live block, as the
        // balance route sums them.
        let balances = balance::db::find_signed_by_ids(connection, &refs, i64::MAX)?;
        wallets
            .into_iter()
            .map(|wallet| {
                let balances = wallet
                    .indexes
                    .iter()
                    .map(|index| {
                        let id = WalletIndex::id(&index.address, &index.asset)?;
                        Ok(balances.get(&id).map(|one| one.confirmed.clone()))
                    })
                    .collect::<anyhow::Result<_>>()?;
                Ok(Entry { wallet, balances })
            })
            .collect()
    })
    .await
}
