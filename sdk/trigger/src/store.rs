//! Where wallet indexes come from: the [`Store`] a round reads, and
//! `CachedStore`, which answers each read once per round.

use std::collections::BTreeMap;

use base::{Address, Asset, Future};

use crate::selection::{Index, Selector};

/// Reads one network's wallet indexes at its confirmed block, all from one
/// consistent read: only fully indexed ones (indexed through a live block at
/// or above the confirmed position, the universe [`Selector::Not`] is
/// relative to), each with its balances at that position.
pub trait Store: Send {
    /// The indexes `selector` picks.
    fn select<'a>(&'a mut self, selector: &'a Selector) -> Future<'a, anyhow::Result<Vec<Index>>>;

    /// Of `wanted`, the fully indexed ones; the rest are left out.
    fn find<'a>(
        &'a mut self,
        wanted: &'a [(Address, Asset)],
    ) -> Future<'a, anyhow::Result<Vec<Index>>>;
}

/// A [`Store`] over another that answers each selector, and each set of
/// wanted pairs, once per round: under one consistent read the answers are
/// equal, so a filter testing many indexes reads a selector once.
pub(crate) struct CachedStore<'s> {
    store: &'s mut dyn Store,
    selected: BTreeMap<Selector, Vec<Index>>,
    found: BTreeMap<Vec<(Address, Asset)>, Vec<Index>>,
}

impl<'s> CachedStore<'s> {
    /// Nothing answered yet, over `store`.
    pub(crate) fn new(store: &'s mut dyn Store) -> Self {
        Self {
            store,
            selected: BTreeMap::new(),
            found: BTreeMap::new(),
        }
    }
}

impl Store for CachedStore<'_> {
    fn select<'a>(&'a mut self, selector: &'a Selector) -> Future<'a, anyhow::Result<Vec<Index>>> {
        Box::pin(async move {
            if let Some(found) = self.selected.get(selector) {
                return Ok(found.clone());
            }
            let found = self.store.select(selector).await?;
            self.selected.insert(selector.clone(), found.clone());
            Ok(found)
        })
    }

    fn find<'a>(
        &'a mut self,
        wanted: &'a [(Address, Asset)],
    ) -> Future<'a, anyhow::Result<Vec<Index>>> {
        Box::pin(async move {
            if let Some(found) = self.found.get(wanted) {
                return Ok(found.clone());
            }
            let found = self.store.find(wanted).await?;
            self.found.insert(wanted.to_vec(), found.clone());
            Ok(found)
        })
    }
}
