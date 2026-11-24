//! [`Memory`]: every base store on one shared state.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
};

use anyhow::Result;
use crypto::SecretBytes;

use crate::{
    Address, Addressed, BlockHeader, BlockStore, Blocks, Confirmed, Future, Holding, Holdings,
    IndexState, Indexed, InputStore, Inputs, IntoIndex, Kept, Leg, Network, Storage, Transaction,
    TransactionIndex, TransactionIndexStore, TransactionState, Wallet, WalletFactories,
    WalletIndex, WalletIndexStore, WalletStore,
};

/// The base stores in memory, on one state: kept blocks, the indexes of
/// both kinds, unspent inputs, sends in flight and wallet keys. It keeps
/// the progress rules the indexer relies on: an insert moves the indexes
/// its blocks apply to (and notes where a transaction is found), a delete
/// rewinds both kinds, and the index writes. It keeps no legs, totals,
/// coins or adjustments: those are the server's SQL.
#[derive(Default)]
pub struct Memory(Mutex<State>);

#[derive(Default)]
struct State {
    blocks: BTreeMap<Network, BTreeMap<u64, BlockHeader>>,
    wallet_indexes: Vec<WalletIndex>,
    transaction_indexes: Vec<TransactionIndex>,
    unspent: Vec<crate::Input>,
    sent: Vec<Transaction>,
    wallets: Vec<(Network, Vec<u8>)>,
}

impl Memory {
    /// Empty stores.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every store on this one state, pricing sends with `holdings`.
    #[must_use]
    pub fn storage(self: &Arc<Self>, holdings: Arc<dyn Holdings>) -> Storage {
        Storage {
            blocks: self.clone(),
            indexes: self.clone(),
            transaction_indexes: self.clone(),
            inputs: self.clone(),
            wallets: self.clone(),
            holdings,
        }
    }

    /// Keeps `inputs` unspent.
    pub fn unspent(&self, inputs: impl IntoIterator<Item = crate::Input>) {
        self.state().unspent.extend(inputs);
    }

    /// Keeps `transaction` in flight ([`TransactionIndexStore::find_sent`]).
    pub fn sent(&self, transaction: Transaction) {
        self.state().sent.push(transaction);
    }

    /// The kept wallet index of the address whose text is `address`.
    ///
    /// # Panics
    /// When none is kept.
    #[must_use]
    pub fn wallet_index(&self, address: &str) -> WalletIndex {
        let state = self.state();
        let found = state
            .wallet_indexes
            .iter()
            .find(|index| index.address.value == address);
        found.cloned().expect("a wallet index")
    }

    fn state(&self) -> MutexGuard<'_, State> {
        super::lock(&self.0)
    }

    /// Applies `change` to the kept wallet index `index` names.
    fn wallet(&self, index: &WalletIndex, change: impl FnOnce(&mut WalletIndex)) {
        let mut state = self.state();
        let kept = state
            .wallet_indexes
            .iter_mut()
            .find(|kept| kept.address == index.address && kept.asset == index.asset);
        kept.map(change);
    }

    /// Applies `change` to the kept transaction index `index` names.
    fn transaction(&self, index: &TransactionIndex, change: impl FnOnce(&mut TransactionIndex)) {
        let mut state = self.state();
        let kept = state
            .transaction_indexes
            .iter_mut()
            .find(|kept| kept.transaction == index.transaction);
        kept.map(change);
    }
}

/// Whether block `at` moves `index`: it indexes, `at` is at or above its
/// start, and `at` links onto where it read through (or, unstarted, `at`'s
/// parent is below its start).
fn applies(index: &impl Indexed, at: &BlockHeader) -> bool {
    let progress = index.progress();
    if progress.state != IndexState::Indexing || index.next().is_none() {
        return false;
    }
    if index.start() > at.position {
        return false;
    }
    let parent = at.parent.as_ref().map(|parent| parent.position);
    match progress.indexed_at {
        Some(through) => parent.is_some_and(|parent| parent <= through) && through < at.position,
        None => parent.is_none_or(|parent| parent < index.start()),
    }
}

/// `indexed_at` capped below `from`: unset when that is below `start`.
fn rewound(indexed_at: Option<u64>, from: u64, start: u64) -> Option<u64> {
    match indexed_at {
        Some(through) if through >= from => from.checked_sub(1).filter(|kept| *kept >= start),
        through => through,
    }
}

/// Async `value`.
fn ready<'a, T: Send + 'a>(value: T) -> Future<'a, Result<T>> {
    Box::pin(async move { Ok(value) })
}

impl BlockStore for Memory {
    fn insert<'a>(&'a self, run: &'a Blocks) -> Future<'a, Result<Kept>> {
        let mut state = self.state();
        let state = &mut *state;
        for block in run.iter() {
            let at = &block.header;
            let kept = state.blocks.entry(block.network.clone()).or_default();
            if let Some(held) = kept.get(&at.position)
                && held.hash != at.hash
            {
                return Box::pin(async { anyhow::bail!("a kept block conflicts") });
            }
        }
        for block in run.iter() {
            let at = &block.header;
            let network = &block.network;
            for index in &mut state.wallet_indexes {
                if index.address.network == *network && applies(index, at) {
                    index.progress.indexed_at = Some(at.position);
                }
            }
            for index in &mut state.transaction_indexes {
                if index.transaction.network != *network || !applies(index, at) {
                    continue;
                }
                index.progress.indexed_at = Some(at.position);
                let mut mined = block.transactions.0.iter();
                if let Some(found) = mined.find(|t| t.id == index.transaction) {
                    index.found = Some(at.position);
                    index.reverted = matches!(found.state, TransactionState::Reverted(_));
                }
            }
            let kept = state.blocks.entry(network.clone()).or_default();
            kept.insert(at.position, at.clone());
        }
        ready(Kept::default())
    }

    fn find_last<'a>(&'a self, network: &'a Network) -> Future<'a, Result<Option<BlockHeader>>> {
        let state = self.state();
        let kept = state.blocks.get(network);
        let last = kept.and_then(|kept| kept.values().next_back().cloned());
        ready(last)
    }

    fn delete<'a>(&'a self, network: &'a Network, from: u64) -> Future<'a, Result<()>> {
        let mut state = self.state();
        if let Some(kept) = state.blocks.get_mut(network) {
            kept.retain(|position, _| *position < from);
        }
        for index in &mut state.wallet_indexes {
            if index.address.network == *network {
                let progress = &mut index.progress;
                progress.indexed_at = rewound(progress.indexed_at, from, index.birthday);
            }
        }
        for index in &mut state.transaction_indexes {
            if index.transaction.network != *network {
                continue;
            }
            let progress = &mut index.progress;
            progress.indexed_at = rewound(progress.indexed_at, from, index.broadcasted_at);
            if index.found.is_some_and(|found| found >= from) {
                (index.found, index.reverted) = (None, false);
            }
        }
        ready(())
    }
}

impl WalletIndexStore for Memory {
    fn insert<'a>(&'a self, index: &'a dyn IntoIndex<WalletIndex>) -> Future<'a, Result<()>> {
        Box::pin(async move {
            let index = index.index().await?;
            let mut state = self.state();
            let indexes = &mut state.wallet_indexes;
            let kept = indexes
                .iter()
                .any(|kept| kept.address == index.address && kept.asset == index.asset);
            if !kept {
                indexes.push(index);
            }
            Ok(())
        })
    }

    fn find_all<'a>(&'a self, network: &'a Network) -> Future<'a, Result<Vec<WalletIndex>>> {
        let state = self.state();
        let all = state.wallet_indexes.iter();
        let of = all.filter(|index| index.address.network == *network);
        ready(of.cloned().collect())
    }

    fn move_start<'a>(&'a self, index: &'a WalletIndex, start: u64) -> Future<'a, Result<()>> {
        self.wallet(index, |kept| {
            if kept.progress.indexed_at.is_none() {
                kept.birthday = start;
            }
        });
        ready(())
    }

    fn set_state<'a>(
        &'a self,
        index: &'a WalletIndex,
        state: IndexState,
        reason: Option<&'a str>,
    ) -> Future<'a, Result<()>> {
        self.wallet(index, |kept| {
            kept.progress.state = state;
            kept.progress.reason = reason.map(str::to_owned);
        });
        ready(())
    }

    /// Continues `index` from the position after `holding`'s.
    fn settle<'a>(
        &'a self,
        index: &'a WalletIndex,
        holding: &'a Holding,
    ) -> Future<'a, Result<bool>> {
        self.wallet(index, |kept| {
            kept.progress.indexed_at = Some(holding.position)
        });
        ready(true)
    }

    fn restart<'a>(&'a self, index: &'a WalletIndex, birthday: u64) -> Future<'a, Result<()>> {
        self.wallet(index, |kept| {
            kept.birthday = birthday;
            kept.progress = crate::Progress::default();
        });
        ready(())
    }
}

impl TransactionIndexStore for Memory {
    fn insert<'a>(&'a self, index: &'a dyn IntoIndex<TransactionIndex>) -> Future<'a, Result<()>> {
        Box::pin(async move {
            let index = index.index().await?;
            let mut state = self.state();
            let indexes = &mut state.transaction_indexes;
            if !indexes
                .iter()
                .any(|kept| kept.transaction == index.transaction)
            {
                indexes.push(index);
            }
            Ok(())
        })
    }

    fn find_all<'a>(&'a self, network: &'a Network) -> Future<'a, Result<Vec<TransactionIndex>>> {
        let state = self.state();
        let all = state.transaction_indexes.iter();
        let of = all.filter(|index| index.transaction.network == *network);
        ready(of.cloned().collect())
    }

    fn move_start<'a>(&'a self, index: &'a TransactionIndex, start: u64) -> Future<'a, Result<()>> {
        self.transaction(index, |kept| {
            if kept.progress.indexed_at.is_none() {
                kept.broadcasted_at = start;
            }
        });
        ready(())
    }

    fn set_state<'a>(
        &'a self,
        index: &'a TransactionIndex,
        state: IndexState,
        reason: Option<&'a str>,
    ) -> Future<'a, Result<()>> {
        self.transaction(index, |kept| {
            kept.progress.state = state;
            kept.progress.reason = reason.map(str::to_owned);
        });
        ready(())
    }

    fn settle<'a>(
        &'a self,
        index: &'a TransactionIndex,
        confirmed: &'a Confirmed,
    ) -> Future<'a, Result<()>> {
        self.transaction(index, |kept| {
            kept.progress.indexed_at = Some(confirmed.position);
            if confirmed.found.is_some() {
                (kept.found, kept.reverted) = (confirmed.found, confirmed.reverted);
            }
        });
        ready(())
    }

    fn give_up<'a>(&'a self, index: &'a TransactionIndex) -> Future<'a, Result<()>> {
        self.transaction(index, |kept| kept.progress.state = IndexState::NoData);
        let id = &index.transaction;
        self.state().sent.retain(|sent| sent.id != *id);
        ready(())
    }

    fn delete<'a>(&'a self, index: &'a TransactionIndex) -> Future<'a, Result<()>> {
        let id = &index.transaction;
        let mut state = self.state();
        state
            .transaction_indexes
            .retain(|kept| kept.transaction != *id);
        ready(())
    }

    fn find_sent<'a>(&'a self, sender: &'a Address) -> Future<'a, Result<Vec<Transaction>>> {
        let pays = |leg: &Leg| {
            matches!(
                leg,
                Leg::Spent { .. } | Leg::Burned { .. } | Leg::Fee { .. }
            ) && leg.address() == sender
        };
        let state = self.state();
        let sent = state.sent.iter().filter(|sent| sent.legs.iter().any(pays));
        ready(sent.cloned().collect())
    }
}

impl InputStore for Memory {
    fn find_unspent<'a>(&'a self, addresses: &'a [Address]) -> Future<'a, Result<Inputs>> {
        let state = self.state();
        let of = state
            .unspent
            .iter()
            .filter(|input| addresses.contains(&input.address));
        ready(of.cloned().collect::<Vec<_>>().into())
    }
}

impl WalletStore for Memory {
    fn insert<'a>(&'a self, wallet: &'a dyn Wallet) -> Future<'a, Result<()>> {
        let (_, secret) = wallet.private_key();
        let key = (wallet.network().clone(), secret.as_bytes().to_vec());
        self.state().wallets.push(key);
        ready(())
    }

    fn find_all<'a>(
        &'a self,
        factories: &'a WalletFactories,
    ) -> Future<'a, Result<Vec<Arc<dyn Wallet>>>> {
        let keys = self.state().wallets.clone();
        Box::pin(async move {
            let mut wallets = Vec::new();
            for (network, secret) in keys {
                wallets.push(
                    factories
                        .from_pk(&network, SecretBytes::new(secret))
                        .await?,
                );
            }
            Ok(wallets)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Asset, Birthday, Chain, WalletIndexTarget,
        testing::{Served, fixtures},
    };

    /// Stores watching `a` from 2 on a quiet chain of 0..=9.
    async fn watching() -> (Memory, Network) {
        let network = Network::new(Chain::from_static("test"), "memory");
        let memory = Memory::new();
        let target = WalletIndexTarget::new(
            Arc::new(Served::new(fixtures::quiet(&network, 9), 0)),
            Address::new(network.clone(), "a"),
            Asset::native(network.clone()),
        )
        .with_birthday(Birthday::At(2));
        WalletIndexStore::insert(&memory, &target).await.unwrap();
        (memory, network)
    }

    /// Blocks `from..=to` of branch `fork`, linked.
    fn blocks(network: &Network, from: u64, to: u64, fork: u8) -> Blocks {
        let blocks = (from..=to).map(|p| fixtures::block(network, p, fork, vec![]));
        fixtures::run(network, blocks.collect())
    }

    #[tokio::test]
    async fn an_insert_moves_an_index_only_from_its_start_and_onto_its_block() {
        let (memory, network) = watching().await;
        BlockStore::insert(&memory, &blocks(&network, 0, 1, 0))
            .await
            .unwrap();
        assert_eq!(memory.wallet_index("a").progress.indexed_at, None, "below");
        BlockStore::insert(&memory, &blocks(&network, 2, 4, 0))
            .await
            .unwrap();
        assert_eq!(memory.wallet_index("a").progress.indexed_at, Some(4));
        BlockStore::insert(&memory, &blocks(&network, 6, 7, 0))
            .await
            .unwrap();
        assert_eq!(
            memory.wallet_index("a").progress.indexed_at,
            Some(4),
            "a gap"
        );
        let conflict = BlockStore::insert(&memory, &blocks(&network, 4, 5, 1)).await;
        assert!(conflict.is_err(), "another block at 4");
    }

    #[tokio::test]
    async fn a_delete_rewinds_an_index_below_it() {
        let (memory, network) = watching().await;
        BlockStore::insert(&memory, &blocks(&network, 0, 6, 0))
            .await
            .unwrap();
        BlockStore::delete(&memory, &network, 5).await.unwrap();
        assert_eq!(memory.wallet_index("a").progress.indexed_at, Some(4));
        assert_eq!(
            memory.find_last(&network).await.unwrap(),
            Some(fixtures::header(4, 0))
        );
        BlockStore::delete(&memory, &network, 1).await.unwrap();
        assert_eq!(
            memory.wallet_index("a").progress.indexed_at,
            None,
            "below its start"
        );
    }

    #[tokio::test]
    async fn a_settle_continues_an_index_after_the_settled_block() {
        let (memory, network) = watching().await;
        let index = memory.wallet_index("a");
        let holding = Holding {
            position: 6,
            amount: crate::Decimal::from(1_u64),
        };
        assert!(
            WalletIndexStore::settle(&memory, &index, &holding)
                .await
                .unwrap()
        );
        BlockStore::insert(&memory, &blocks(&network, 7, 8, 0))
            .await
            .unwrap();
        assert_eq!(memory.wallet_index("a").progress.indexed_at, Some(8));
    }
}
