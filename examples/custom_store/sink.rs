//! The store this scenario supplies, and the whole of what `indexing` asks of
//! one.
//!
//! **Two ports are required**, and each has a block of its own below, labelled
//! with which it is: [`WatchStore`] is the durable list of what is observed and
//! how far each observation got, and [`IndexStore`] is the network's checkpoint
//! plus the one write a pass makes. Nothing else is required, and nothing else
//! is implemented here.
//!
//! **Three more traits exist and this file implements none of them** —
//! `indexing::BalanceStore`, `indexing::OutputStore` and `indexing::Purging`:
//! *consumer* capabilities, for the HTTP balance, Bitcoin's change selection and
//! the offline rescan. There is no port for a history page at all — querying is
//! a repository's own API (`indexing::ConfirmedHistory`), and this one does not
//! offer it. What this consumer needs instead is the last block of this file:
//! three questions in its own words, asked of the store directly.
//!
//! It is deliberately not [`Clone`]. An index clones the handle it was assembled
//! with, so the scenario holds one store as `Arc<Sink>`, and `indexing`
//! implements every port for `Arc<T>` by forwarding.
//!
//! Atomicity is expressed the one way a map-backed store can: a write mutates a
//! copy and installs it, so a refused write leaves every row as it was — and a
//! re-anchor, which is a withdrawal and a registration in one, is never seen
//! half done.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

use indexing::{
    Address, AssetId, BlockCredit, BlockPosition, BlockRef, BoxFuture, Decimal, IndexError,
    IndexErrorKind, IndexStore, IndexedBatch, Network, OutputChanges, OutputId, OutputKey,
    Progress, Transaction, Transactions, Watch, WatchProgress, WatchStore,
};

/// One store, serving every network it is handed to; not `Clone`, because the
/// scenario holds it as `Arc<Sink>`.
#[derive(Default)]
pub struct Sink {
    rows: Mutex<Rows>,
}

/// Every row the store holds, keyed by the domain values themselves.
#[derive(Clone, Default)]
struct Rows {
    checkpoints: BTreeMap<Network, BlockRef>,
    /// Identities are unique store-wide, as a SQL store's unique index on the id.
    ids: BTreeMap<String, Network>,
    watches: BTreeMap<(Network, AssetId, String), Watch>,
    /// How far each observation has been inspected, by its own identity: a claim
    /// is earned under that observation's own birthday and belongs to nothing else.
    progress: BTreeMap<(Network, String), BlockRef>,
    live: BTreeSet<(Network, String, OutputId)>,
    /// Every outpoint a required spend named, so no historical write puts one back.
    spent: BTreeSet<(Network, String, OutputId)>,
    landed: BTreeMap<(Network, String), Landing>,
    /// Which `(address, block)` pairs have already been folded into `landed`. A
    /// store that keeps no history row has nothing for an insert-or-verify commit
    /// to verify against, so it owes idempotence some other way.
    counted: BTreeSet<(Network, String, BlockPosition)>,
}

/// What has landed on one observed address, as this consumer counts it.
///
/// Not a balance and not a history page: a deposit watcher wants the total that
/// arrived, how many blocks brought it, and the newest of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Landing {
    /// Every amount credited to the address by a stored block, summed in the
    /// asset's smallest unit.
    pub credited: Decimal,
    /// How many stored blocks credited it.
    pub blocks: usize,
    /// The newest block position that credited it.
    pub newest: BlockPosition,
}

impl Sink {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads under the lock. A poisoned lock is a terminal store failure.
    fn read<T>(&self, read: impl FnOnce(&Rows) -> Result<T, IndexError>) -> Result<T, IndexError> {
        let rows = self.rows.lock().map_err(|_| poisoned())?;
        read(&rows)
    }

    /// Applies `write` to a copy of the rows, installing it only if it succeeds.
    fn apply<T>(
        &self,
        write: impl FnOnce(&mut Rows) -> Result<T, IndexError>,
    ) -> Result<T, IndexError> {
        let mut rows = self.rows.lock().map_err(|_| poisoned())?;
        let mut staged = rows.clone();
        let written = write(&mut staged)?;
        *rows = staged;
        Ok(written)
    }

    /// Where one address's rows are filed.
    fn at(address: &Address) -> (Network, String) {
        (address.network.clone(), address.value.clone())
    }
}

// ---------------------------------------------------------------------------
// REQUIRED PORT 1 of 2: `indexing::WatchStore`.
//
// The durable list of what this store observes and how far each observation has
// been inspected; the half a restart depends on.
// ---------------------------------------------------------------------------

impl WatchStore for Sink {
    fn add<'a>(&'a self, watches: Vec<Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            Self::one_network(&watches)?;
            // One `apply` for the whole batch: the staged copy is installed only
            // if every watch was accepted, so a conflict leaves no row behind.
            self.apply(|rows| Self::accept(rows, &watches))
        })
    }

    fn all<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Vec<WatchProgress>, IndexError>> {
        // One `read`, so a watch is never paired with another watch's coverage.
        Box::pin(async move {
            self.read(|rows| {
                Ok(rows
                    .watches
                    .values()
                    .filter(|watch| watch.address.belongs_to(network))
                    .map(|watch| Self::recorded(rows, watch))
                    .collect())
            })
        })
    }

    fn forget<'a>(
        &'a self,
        network: &'a Network,
        watch: &'a Watch,
    ) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            if !watch.address.belongs_to(network) {
                return Err(IndexError::new(
                    IndexErrorKind::NetworkMismatch,
                    "watch belongs to another network",
                    false,
                ));
            }
            self.apply(|rows| {
                Self::withdraw(rows, watch);
                Ok(())
            })
        })
    }

    fn cancel_backfill<'a>(&'a self, watches: Vec<Watch>) -> BoxFuture<'a, Result<(), IndexError>> {
        Box::pin(async move {
            Self::one_network(&watches)?;
            // The withdrawal and the registration are staged together and
            // installed once, so no reader ever sees the store without the row
            // and no crash can leave the address unwatched.
            self.apply(|rows| Self::renewed(rows, &watches))
        })
    }
}

impl Sink {
    /// One observation and whatever has been inspected for it, which is the pair
    /// a restart needs: neither is usable without the other.
    fn recorded(rows: &Rows, watch: &Watch) -> WatchProgress {
        WatchProgress {
            indexed_through: rows.progress.get(&Self::filed(watch)).cloned(),
            watch: watch.clone(),
        }
    }

    /// Where one observation's progress is filed.
    fn filed(watch: &Watch) -> (Network, String) {
        (watch.address.network.clone(), watch.id.clone())
    }

    /// Refuses a batch this store cannot write as one change.
    ///
    /// One network is the only rule a store owns here: the staged copy is
    /// installed once, so two networks would be one write over rows of both. A
    /// row's own address and asset are [`Watch::validate`]'s to answer for.
    fn one_network(watches: &[Watch]) -> Result<(), IndexError> {
        let Some(first) = watches.first() else {
            return Ok(());
        };
        for watch in watches {
            watch.validate()?;
            if watch.address.network != first.address.network {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidRequest,
                    "a batch of watches must all belong to one network",
                    false,
                ));
            }
        }
        Ok(())
    }

    /// Registers every watch of the batch, or none of them.
    fn accept(rows: &mut Rows, watches: &[Watch]) -> Result<(), IndexError> {
        for watch in watches {
            Self::register(rows, watch)?;
        }
        Ok(())
    }

    /// Insert-if-absent, verify-if-present, for one watch of the batch.
    ///
    /// An exact repeat keeps whatever progress the row already earned, which is
    /// how a caller resolves an `add` whose outcome it never saw. Anything else
    /// under a name that is taken would retarget progress the other observation
    /// earned, so it is refused.
    fn register(rows: &mut Rows, watch: &Watch) -> Result<(), IndexError> {
        let (network, address) = Self::at(&watch.address);
        let key = (network.clone(), watch.asset.clone(), address);
        match (rows.watches.get(&key), rows.ids.get(&watch.id)) {
            (Some(stored), _) if stored == watch => Ok(()),
            (Some(_), _) | (None, Some(_)) => Err(IndexError::new(
                IndexErrorKind::Conflict,
                "this identity or this address is already observed",
                false,
            )),
            (None, None) => {
                rows.ids.insert(watch.id.clone(), network);
                rows.watches.insert(key, watch.clone());
                Ok(())
            }
        }
    }

    /// Every observation of the batch at the birthday it now carries, with the
    /// claim it earned under the old one gone.
    fn renewed(rows: &mut Rows, watches: &[Watch]) -> Result<(), IndexError> {
        for watch in watches {
            Self::withdraw(rows, watch);
        }
        Self::accept(rows, watches)
    }

    /// Removes one observation, and the progress it earned.
    ///
    /// Idempotent, and nothing indexed is deleted, which is what makes the
    /// withdrawal reversible. Progress is per observation, so another observation
    /// of the same address keeps its own claim.
    fn withdraw(rows: &mut Rows, watch: &Watch) {
        let (network, address) = Self::at(&watch.address);
        let key = (network, watch.asset.clone(), address);
        if rows.watches.remove(&key).is_some() {
            rows.ids.remove(&watch.id);
        }
        rows.progress.remove(&Self::filed(watch));
    }
}

// ---------------------------------------------------------------------------
// REQUIRED PORT 2 of 2: `indexing::IndexStore`.
//
// The network's checkpoint, and the one write a pass makes: a batch of blocks and
// the per-observation progress they prove. Whether the confirmed line moves is a
// property of the blocks, so a re-inspected older block goes through the same
// call; the checkpoint and the rows a write conditions on are read inside it.
// ---------------------------------------------------------------------------

impl IndexStore for Sink {
    fn checkpoint<'a>(
        &'a self,
        network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move { self.read(|rows| Ok(rows.checkpoints.get(network).cloned())) })
    }

    fn commit<'a>(
        &'a self,
        batch: IndexedBatch,
    ) -> BoxFuture<'a, Result<Vec<OutputKey>, IndexError>> {
        Box::pin(async move { self.apply(|rows| Self::write(rows, &batch)) })
    }
}

impl Sink {
    /// One batch: its blocks, the claims it proves and the line it leaves behind.
    fn write(rows: &mut Rows, batch: &IndexedBatch) -> Result<Vec<OutputKey>, IndexError> {
        let network = batch.network();
        if rows.checkpoints.get(network) != batch.expected_checkpoint() {
            return Err(moved("the checkpoint moved before this batch"));
        }
        for claim in batch.progress() {
            Self::advance(rows, network, claim)?;
        }
        let mut unknown = Vec::new();
        for block in batch.blocks() {
            let at = block.block.reference().position;
            Self::land(rows, at, block.block.transactions(), &block.credits);
            unknown.extend(Self::settle(rows, network, &block.outputs));
        }
        if let Some(line) = batch.checkpoint() {
            rows.checkpoints.insert(network.clone(), line.clone());
        }
        Ok(unknown)
    }

    /// Records one claim, after comparing what it expected against what is held.
    ///
    /// Deliberately not a monotonic rule: a batch redone under a smaller budget
    /// can settle less than the write it is redoing, so what is refused is a claim
    /// whose author was reading a different value.
    fn advance(rows: &mut Rows, network: &Network, claim: &Progress) -> Result<(), IndexError> {
        if rows.ids.get(&claim.watch_id) != Some(network) {
            return Err(moved("this batch names an observation nothing here holds"));
        }
        let key = (network.clone(), claim.watch_id.clone());
        if rows.progress.get(&key) != claim.expected.as_ref() {
            return Err(moved("recorded progress changed before this batch"));
        }
        rows.progress.insert(key, claim.indexed_through.clone());
        Ok(())
    }

    /// Folds what one block credited to each address this store observes, once
    /// per `(address, block)` pair.
    ///
    /// The guard is what makes a re-presented batch harmless: a store holding a
    /// row per transaction would verify the row, and this one holds none. The
    /// block's **total** per address is therefore formed first — a movement's
    /// identity is unique within its block and not within its block and address,
    /// so a guard taken per credit would drop a second credit to one address.
    fn land(
        rows: &mut Rows,
        position: BlockPosition,
        transactions: &Transactions<Transaction>,
        credits: &[BlockCredit],
    ) {
        let sent = transactions
            .iter()
            .flat_map(|transaction| transaction.movements.iter())
            .filter_map(|movement| movement.to().map(|to| (to, &movement.amount)));
        let block = credits.iter().map(|credit| (&credit.to, &credit.amount));
        let mut totals: BTreeMap<(Network, String), Decimal> = BTreeMap::new();
        for (to, amount) in sent.chain(block).filter(|(to, _)| Self::observes(rows, to)) {
            let running = totals.entry(Self::at(to)).or_insert_with(Decimal::zero);
            *running = Decimal::total([&running.clone(), amount]);
        }
        for ((network, address), amount) in totals {
            let counted = (network.clone(), address.clone(), position);
            if !rows.counted.insert(counted) {
                continue;
            }
            let landing = rows.landed.entry((network, address)).or_insert(Landing {
                credited: Decimal::zero(),
                blocks: 0,
                newest: position,
            });
            landing.credited = Decimal::total([&landing.credited, &amount]);
            landing.blocks = landing.blocks.saturating_add(1);
            landing.newest = landing.newest.max(position);
        }
    }

    /// Applies one block's output changes, reporting every required spend this
    /// store holds no live output for.
    ///
    /// The spend outlives the block that reported it, **matched or not**: no
    /// batch carries that spend again, while the block that created the output
    /// may still be read from below, so a creation the record names is refused.
    fn settle(rows: &mut Rows, network: &Network, changes: &OutputChanges) -> Vec<OutputKey> {
        for output in &changes.created {
            let key = Self::output(network, &output.key());
            if !rows.spent.contains(&key) {
                rows.live.insert(key);
            }
        }
        let required = changes.spent.iter().map(|key| (key, true));
        let tracked = changes.tracked_spends.iter().map(|key| (key, false));
        let mut unknown = Vec::new();
        for (spent, required) in required.chain(tracked) {
            let key = Self::output(network, spent);
            let held = rows.live.remove(&key);
            if !required {
                continue;
            }
            rows.spent.insert(key);
            if !held {
                unknown.push(spent.clone());
            }
        }
        unknown
    }

    /// Whether this store observes `address` under any asset.
    fn observes(rows: &Rows, address: &Address) -> bool {
        let (network, value) = Self::at(address);
        rows.watches
            .keys()
            .any(|(bound, _, held)| bound == &network && held == &value)
    }

    /// Where one live output is filed.
    fn output(network: &Network, key: &OutputKey) -> (Network, String, OutputId) {
        (
            network.clone(),
            key.address.value.clone(),
            key.output.clone(),
        )
    }
}

// ---------------------------------------------------------------------------
// THIS CONSUMER'S OWN QUERY API.
//
// Not `indexing::History` and not `indexing::Balances`: three plain questions in
// the vocabulary of a deposit watcher. This is what "the queries are a consumer
// capability" means once a consumer writes its own store — it implements no read
// port it never wanted, it just asks.
// ---------------------------------------------------------------------------

impl Sink {
    /// Every address observed on `network`, by the id it was filed under, with the
    /// native block position it is observed from.
    #[must_use]
    pub fn observed(&self, network: &Network) -> Vec<(String, BlockPosition)> {
        self.asked(network, |_, watch| Some(watch.start))
    }

    /// What has landed on each observed address of `network`, by id; an address
    /// nothing has credited is absent.
    #[must_use]
    pub fn landings(&self, network: &Network) -> Vec<(String, Landing)> {
        self.asked(network, |rows, watch| {
            rows.landed.get(&Self::at(&watch.address)).cloned()
        })
    }

    /// Each observation of `network` still reading its own past, by id, with the
    /// first block it still needs.
    ///
    /// One question, asked of the index's own vocabulary:
    /// `WatchProgress::caught_up` is the predicate the pass and the shipped
    /// stores' own claims use, so a consumer asking "who is behind" gets the
    /// same answer they do rather than a second opinion.
    #[must_use]
    pub fn catching_up(&self, network: &Network) -> Vec<(String, BlockPosition)> {
        let line = self
            .read(|rows| Ok(rows.checkpoints.get(network).cloned()))
            .unwrap_or_default();
        self.asked(network, |rows, watch| {
            let recorded = Self::recorded(rows, watch);
            if recorded.caught_up(line.as_ref()) {
                return None;
            }
            recorded.next_required()
        })
    }

    /// Every observation of `network` in id order, each with whatever `about`
    /// derives for it, and nothing for the ones it derives nothing for.
    ///
    /// A poisoned lock answers the empty list rather than a failure: this is a
    /// report, and nothing here depends on telling empty from unreadable.
    fn asked<T>(
        &self,
        network: &Network,
        about: impl Fn(&Rows, &Watch) -> Option<T>,
    ) -> Vec<(String, T)> {
        let Ok(guard) = self.rows.lock() else {
            return Vec::new();
        };
        let rows = &*guard;
        rows.watches
            .values()
            .filter(|watch| watch.address.belongs_to(network))
            .filter_map(|watch| Some((watch.id.clone(), about(rows, watch)?)))
            .collect()
    }
}

/// A lock this store can no longer trust.
fn poisoned() -> IndexError {
    IndexError::new(IndexErrorKind::Store, "the store lock is poisoned", false)
}

/// The checkpoint moved out from under a write: retryable, because the pass reads
/// it again.
fn moved(message: &str) -> IndexError {
    IndexError::new(IndexErrorKind::Conflict, message, true)
}
