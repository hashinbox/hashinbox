//! [`Wallets`]: one network's wallet indexes held in memory, read as a
//! [`Store`].

use base::{Address, Asset, Future};

use crate::{
    selection::{Index, Selector},
    store::Store,
};

/// One network's wallet indexes, held in memory: each with its wallet's
/// tags, fully indexed or behind.
///
/// Only fully indexed ones are ever answered, so they are the universe a
/// [`Selector::Not`] is relative to, and a `find` leaves the others out.
/// Answers come sorted by id, as the server's store answers them.
#[derive(Clone, Debug, Default)]
pub struct Wallets {
    entries: Vec<Entry>,
    reads: usize,
}

/// One index, its wallet's tags, and whether it is fully indexed.
#[derive(Clone, Debug)]
struct Entry {
    index: Index,
    tags: Vec<String>,
    indexed: bool,
}

impl Wallets {
    /// None yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `index`, fully indexed, its wallet tagged `tags` (none:
    /// untagged).
    pub fn add(&mut self, index: Index, tags: &[&str]) -> &mut Self {
        self.push(index, tags, true)
    }

    /// Adds `index` not yet indexed through the confirmed block: no read
    /// answers it, as if it did not exist.
    pub fn add_behind(&mut self, index: Index, tags: &[&str]) -> &mut Self {
        self.push(index, tags, false)
    }

    /// How many reads (`select` and `find`) it answered.
    #[must_use]
    pub const fn reads(&self) -> usize {
        self.reads
    }

    fn push(&mut self, index: Index, tags: &[&str], indexed: bool) -> &mut Self {
        let tags = tags.iter().map(|tag| (*tag).to_owned()).collect();
        self.entries.push(Entry {
            index,
            tags,
            indexed,
        });
        self
    }

    /// The fully indexed ones that `keep` keeps, by id.
    fn answer(&mut self, keep: impl Fn(&Entry) -> bool) -> Vec<Index> {
        self.reads += 1;
        let mut found: Vec<Index> = self
            .entries
            .iter()
            .filter(|entry| entry.indexed && keep(entry))
            .map(|entry| entry.index.clone())
            .collect();
        found.sort_by(|a, b| a.id.cmp(&b.id));
        found
    }
}

/// Whether `selector` picks `entry`.
fn picks(selector: &Selector, entry: &Entry) -> bool {
    match selector {
        Selector::Ids(ids) => ids.contains(&entry.index.id),
        Selector::Asset(asset) => entry.index.asset.id == *asset,
        Selector::Tag(tag) => entry.tags.contains(tag),
        Selector::Untagged {} => entry.tags.is_empty(),
        Selector::All(all) => all.iter().all(|one| picks(one, entry)),
        Selector::Any(any) => any.iter().any(|one| picks(one, entry)),
        Selector::Not(one) => !picks(one, entry),
    }
}

impl Store for Wallets {
    fn select<'a>(&'a mut self, selector: &'a Selector) -> Future<'a, anyhow::Result<Vec<Index>>> {
        let found = self.answer(|entry| picks(selector, entry));
        Box::pin(std::future::ready(Ok(found)))
    }

    fn find<'a>(
        &'a mut self,
        wanted: &'a [(Address, Asset)],
    ) -> Future<'a, anyhow::Result<Vec<Index>>> {
        let found = self.answer(|entry| {
            let one = &entry.index;
            wanted
                .iter()
                .any(|(address, asset)| one.address == *address && one.asset == *asset)
        });
        Box::pin(std::future::ready(Ok(found)))
    }
}
