//! Every wallet this server knows: each keyed address (the `wallet` model)
//! and each watched one (`wallet_index`), with its indexes, each index's
//! balance (`balance`), how far each is indexed, and its tags (`tag`). One
//! snapshot.

use std::collections::{BTreeMap, BTreeSet};

use base::{Address, Chain, Decimal, Network};

use crate::{
    db::Db,
    models::{balance, tag, wallet, wallet_index, wallet_index::WalletIndex},
};

/// One wallet: an address keyed here or watched, and what is kept of it.
pub(crate) struct Entry {
    /// Its address.
    pub(crate) address: Address,
    /// Whether this server holds its key.
    pub(crate) keyed: bool,
    /// Its indexes, in index order, each with its balance over every live
    /// block when it has one.
    pub(crate) indexes: Vec<(WalletIndex, Option<Decimal>)>,
    /// How far each of its indexes on the networks asked for is indexed, in
    /// the order of those networks.
    pub(crate) progress: Vec<base::WalletIndex>,
    /// Its tags, by name.
    pub(crate) tags: Vec<String>,
}

impl Entry {
    /// `address`, with nothing kept of it yet.
    fn new(address: &Address, keyed: bool) -> Self {
        Self {
            address: address.clone(),
            keyed,
            indexes: Vec::new(),
            progress: Vec::new(),
            tags: Vec::new(),
        }
    }
}

/// Every keyed or watched wallet in address order; the progress read on
/// `networks` only.
///
/// # Errors
///
/// The read, or a stored value that does not decode.
pub(crate) async fn list(db: &Db, networks: Vec<Network>) -> anyhow::Result<Vec<Entry>> {
    // Every wallet, every index, every tag and every index's balance in one
    // query each; every index's progress in one query per network.
    let (keyed, indexes, tags, balances, progress) = db
        .read(move |connection| {
            let keyed: BTreeSet<_> = wallet::db::find_all(connection)?
                .addresses()
                .into_iter()
                .collect();
            let indexes = wallet_index::db::find_every(connection)?;
            let ids: Vec<&str> = indexes.iter().map(|index| index.id.as_str()).collect();
            // Confirmed through every position: every live block, as the
            // balance route sums them.
            let balances = balance::db::find_signed_by_ids(connection, &ids, i64::MAX)?;
            let mut progress = Vec::new();
            for network in &networks {
                progress.extend(wallet_index::db::find_all(connection, network)?);
            }
            let tags = tag::db::find_all(connection)?;
            Ok((keyed, indexes, tags, balances, progress))
        })
        .await?;
    let mut wallets: BTreeMap<Address, Entry> = keyed
        .iter()
        .map(|address| (address.clone(), Entry::new(address, true)))
        .collect();
    for index in indexes {
        let balance = balances.get(&index.id).map(|one| one.confirmed.clone());
        wallets
            .entry(index.address.clone())
            .or_insert_with(|| Entry::new(&index.address, keyed.contains(&index.address)))
            .indexes
            .push((index, balance));
    }
    for index in progress {
        if let Some(wallet) = wallets.get_mut(&index.address) {
            wallet.progress.push(index);
        }
    }
    for tag in tags.iter() {
        let network = Network::new(Chain::new(&tag.chain), &tag.network);
        if let Some(wallet) = wallets.get_mut(&Address::new(network, &tag.address)) {
            wallet.tags.push(tag.tag.clone());
        }
    }
    Ok(wallets.into_values().collect())
}
