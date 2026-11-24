//! The broadcast job (designs 066, 084, 085, 087): one per [`SendIntent`],
//! one payment, owned until it is resolved. It locks its sender on its
//! network (`chain:network.address`), so a sender's runs go one at a time,
//! and a trigger round (the whole network) never runs beside one. Several
//! payments are several jobs.
//!
//! Its states, and what moves each (084, Resolution):
//!
//! - **new** (no state): signed at its fee level and, in one write, held
//!   beyond what is in flight, recorded pending,
//!   watched by its transaction index, the job's state saved → **recorded**.
//!   Short of funds, a sender index missing, an invalid payment: nothing
//!   recorded, failed (never on chain).
//! - **recorded** (not sent): broadcast once. Refused by every node →
//!   *refused*. Accepted, or no sure answer → **sent**.
//! - **sent**: checked every [`Tracking::check`] by the transaction index
//!   of each version sent, which the indexer moves with every block:
//!   - a version found above the final block, or not read that far:
//!     waits (no resend, no bump); the spend stays held. Mined is not done:
//!     a reorg takes the block back, its transaction pending again with its
//!     spend held, and its index reads again → missing;
//!   - in none of the blocks read since it was sent: sent again;
//!   - missing [`Tracking::bump_after`] blocks past the current version's
//!     broadcast (never one that expires) → **bumped**
//!     while [`Tracking::max_bumps`] allows, at the job's fee level and at
//!     least the chain's step up; refused by every node: undone;
//!   - due again with no bump left, or the bump not signable (the fee cap,
//!     short of change, or of funds) → **cancelling**: a cancel in its slot,
//!     paying nothing but a fee back to the sender, at least a tenth over the
//!     last version (Ethereum: zero to itself at the nonce; Bitcoin: the
//!     coins back, RBF), watched as a version; due again, a cancel again a
//!     tenth up. No cancel signable either (the fee cap, a chain that cannot
//!     cancel) → the same bytes again, the job's reason `stuck`, checked on;
//!   - a version found in a final block → *final* (the payment) or
//!     *cancelled* (a cancel); found reverted there → *reverted*;
//!   - a version that can never land (Solana: its index read through the
//!     confirmed position, not found, and the height of the newest kept
//!     block at or below it above its `expires_height`;
//!     [`TransactionIndex::expired`]) is dropped in one
//!     write: its pending row, its legs and its index, so its spend is
//!     released. Another version still pending: watched on. None left: a
//!     new version signed on a fresh blockhash a tenth up, recorded as any
//!     (its own funds check; short: *expired*, `insufficient-funds`) and
//!     sent by the next run, while [`Tracking::max_bumps`] allows; else
//!     *expired*;
//!   - its index given up or gone (a jump read past it unseen) →
//!     *forgotten*.
//!
//! A network whose every node is disabled: any run waits a check and goes
//! again, [`NO_ENABLED_ENDPOINT`] its reason; it never fails for it.
//!
//! Terminal: *final* (done), *reverted*, *expired*, *refused*, *cancelled*,
//! *forgotten* (failed). Every other state is checked again. Bumps off
//! (`max_bumps` 0), or a job queued before its builder's state was kept:
//! never bumped nor cancelled, sent again until it resolves.
//!
//! The job's end, in one write: the payment's pending rows (every version
//! not mined) go with their legs, so what never landed frees its
//! funds; a mined version (the landed one, a reverted one's fee, a landed
//! cancel's fee) stays chain history; every version's transaction index is
//! deleted; the job's state keeps the outcome, the reason and every txid
//! sent: the only record. A rerun never builds again. Between checks a job
//! blocks nobody, so the sender's next payment runs.

use std::{num::NonZeroUsize, sync::Arc, time::Duration};

use anyhow::{Context as _, ensure};
use base::{
    Address, Asset, Chain, Decimal, Fee, FeeLevel, IntoIndex as _, Network, Reason, Tracking,
    TransactionError, TransactionErrorKind, TransactionIndex, TransactionIndexTarget,
    TransactionStatus, Transfer, WalletError,
};
use diesel::SqliteConnection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tasks::{Ack, Context, Job, Policy, Topic, Worker, millis};
use tracing::Instrument as _;

use super::TIMEOUT;
use crate::{
    db::Db,
    models::{
        Task as Row, block, int, network_index, task, transaction,
        transaction::{Transaction, TransactionId},
        transaction_index,
    },
    usecase::{NO_ENABLED_ENDPOINT, Served as _, indexes::transaction as transaction_indexes},
};

/// The job kind.
pub(crate) const KIND: &str = "broadcast";

/// What a caller enqueues: one payment of one asset from one kept wallet,
/// and nothing else.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SendIntent {
    /// The sender's chain.
    pub(crate) chain: String,
    /// The sender's network.
    pub(crate) network: String,
    /// The sender's canonical address.
    pub(crate) address: String,
    /// The asset id within the network.
    pub(crate) asset: String,
    /// The canonical recipient address in the sender's network.
    pub(crate) to: String,
    /// Exact atomic units, canonical decimal text.
    pub(crate) amount: String,
    /// How fast it should land; `normal` when absent.
    #[serde(default)]
    pub(crate) fee: FeeLevel,
}

/// How far a job got: its state, once its transaction is recorded.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Progress {
    /// The current version's id (txid).
    pub(crate) txid: String,
    /// Whether it was sent: a node accepted it, or may hold it.
    pub(crate) sent: bool,
    /// The versions it replaced, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) replaced: Vec<String>,
    /// The replacements it sent, refused ones too.
    #[serde(default)]
    pub(crate) bumps: u32,
    /// What the current version's builder chose (`base::Builder::save`):
    /// what a bump loads. `None` for a job queued before it was kept: it is
    /// watched, never bumped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) builder: Option<Value>,
    /// The versions that are cancels (txids), oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) cancels: Vec<String>,
    /// How it ended; `None` while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<Outcome>,
    /// Why it failed, as the job's reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

/// How a payment was resolved: a terminal state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Outcome {
    /// A version of the payment is in a final block: done.
    Final,
    /// A version is in a final block, reverted: only its fee paid.
    Reverted,
    /// Every version expired (Solana) and no new one could be recorded: no
    /// bump left, or not signable or fundable.
    #[serde(alias = "dead")]
    Expired,
    /// Every node refused it when first sent.
    Refused,
    /// A cancel is in a final block: the payment never will be.
    Cancelled,
    /// Its index was given up or its row forgotten (a jump read past it).
    Forgotten,
}

impl SendIntent {
    /// The sending wallet's address.
    pub(crate) fn sender(&self) -> Address {
        let network = Network::new(Chain::new(&self.chain), &self.network);
        Address::new(network, &self.address)
    }

    /// The asset it sends.
    fn asset(&self) -> Asset {
        Asset {
            network: self.sender().network,
            id: self.asset.clone(),
        }
    }
}

impl Progress {
    /// The id of each version sent on `network`, the current one last.
    fn ids(&self, network: &Network) -> anyhow::Result<Vec<TransactionId>> {
        let versions = self.replaced.iter().chain(std::iter::once(&self.txid));
        Ok(versions
            .map(|txid| TransactionId::new(network, txid))
            .collect::<Result<_, _>>()?)
    }
}

/// The broadcast worker: the container that signs and sends, and the
/// database that records.
pub(crate) struct Broadcast {
    container: Arc<base::Container<crate::usecase::Transport>>,
    db: Db,
}

/// Why a run cannot go on now.
enum Stop {
    /// Transient: the run is tried again.
    Retry(String),
    /// The payment can never go out: the job fails.
    Fail(String),
}

impl From<anyhow::Error> for Stop {
    fn from(error: anyhow::Error) -> Self {
        Self::Retry(format!("{error:#}"))
    }
}

impl Stop {
    /// A builder's failure: transient ones retry, the rest fail.
    fn built(error: TransactionError) -> Self {
        if error.kind.retryable() {
            Self::Retry(error.message)
        } else {
            Self::Fail(format!("invalid: {}", error.message))
        }
    }
}

/// What the one send came to.
enum Sent {
    /// A node accepted it.
    Accepted,
    /// No sure answer: a node may hold it (why).
    Unsure(String),
    /// Mined before it was sent: nothing to send.
    Mined,
    /// Every node refused it (why).
    Refused(String),
    /// Forgotten before it was sent.
    Forgotten,
}

/// Where a payment stands at a check, by the transaction index of each
/// version sent.
enum Watched {
    /// This version (txid) is in a final block.
    Landed(String),
    /// A version is in a final block, reverted: only its fee paid.
    Reverted,
    /// `versions` can never land ([`TransactionIndex::expired`]); `left`:
    /// another version is still pending (the current one, or an older one
    /// while the current was dropped so: the job alone deletes an index).
    Expired {
        versions: Vec<TransactionId>,
        left: bool,
    },
    /// The current version's index was given up (a jump), or is gone with
    /// no version left that is watched.
    Gone,
    /// Not known yet: a version found above the final block, or not read
    /// that far.
    Unknown,
    /// The current version is in none of the blocks read since it was sent,
    /// through the final one.
    Missing,
    /// Due a bump ([`TransactionIndex::bump_due`]): missing, its index read
    /// `bump_after` blocks past its broadcast.
    BumpDue,
}

/// What a bump came to.
enum Bumped {
    /// The replacement is current and sent; why not surely (a node's doubt).
    Sent(Option<String>),
    /// Every node refused the replacement (why): undone, the old version
    /// current again.
    Refused(String),
    /// No replacement, not even a cancel (why): the fee cap, or a chain
    /// that cannot cancel.
    Stuck(String),
}

impl Broadcast {
    /// Signs and sends through `container`, records in `db`.
    pub(crate) const fn new(
        container: Arc<base::Container<crate::usecase::Transport>>,
        db: Db,
    ) -> Self {
        Self { container, db }
    }

    /// How `network`'s payments are checked and bumped; while it is not
    /// built, checked every minute and never bumped.
    fn tracking(&self, network: &Network) -> Tracking {
        self.container
            .service(network)
            .map(|service| service.tracking)
            .unwrap_or_default()
    }

    /// Step 1: signs the payment and, in one write, records it watched by
    /// its transaction index: the job's state from then on.
    async fn build(&self, task: &mut Context<'_, SendIntent, Progress>) -> Result<Progress, Stop> {
        let (signed, builder) = self.sign(task.input()).await?;
        let index = self.index(&signed).await?;
        let progress = Progress {
            txid: signed.id.id.clone(),
            sent: false,
            replaced: Vec::new(),
            bumps: 0,
            builder: Some(builder),
            cancels: Vec::new(),
            outcome: None,
            reason: None,
        };
        let mut pending = Transaction::try_from(&signed)?;
        let recorded = self.record(task, progress.clone(), move |connection| {
            transaction_indexes::record(connection, &mut pending, &index)
        });
        if let Err(refused) = recorded.await? {
            return Err(Stop::Fail(refused.to_string()));
        }
        Ok(progress)
    }

    /// The payment of `intent`, signed by the kept wallet of its sender at
    /// its fee level, and its builder's state.
    async fn sign(&self, intent: &SendIntent) -> Result<(base::Transaction, Value), Stop> {
        let sender = intent.sender();
        let wallet = self.container.wallet(&sender).await.map_err(|error| {
            match error.downcast_ref::<WalletError>() {
                Some(wallet) if !wallet.kind.retryable() => {
                    Stop::Fail(format!("invalid: {}", wallet.message))
                }
                _ => Stop::from(error),
            }
        })?;
        let amount = intent
            .amount
            .parse::<Decimal>()
            .map_err(|_| Stop::Fail("invalid: amount is not a decimal".into()))?;
        let to = Address::new(sender.network, intent.to.clone());
        let mut builder = wallet.transaction(&intent.asset()).map_err(Stop::built)?;
        builder
            .from_transfer(Transfer::new(to, amount))
            .map_err(Stop::built)?;
        builder.with_fee(Fee::Set(intent.fee));
        let signed = builder.sign().await.map_err(Stop::built)?;
        Ok((signed, builder.save().map_err(Stop::built)?))
    }

    /// The index `signed` is watched with (085): from the tip now, until
    /// its `expires_height` when it has one.
    async fn index(&self, signed: &base::Transaction) -> anyhow::Result<TransactionIndex> {
        let chain = self.container.service(&signed.id)?.indexer.chain();
        let target = TransactionIndexTarget::new(chain, signed.id.clone());
        let target = match signed.expires_height {
            Some(height) => target.with_expires_height(height),
            None => target,
        };
        target.index().await
    }

    /// In one write: `write` (the payment's rows), then the job stored with
    /// `progress` as its state, while the run still holds it; then saved on
    /// `task` too, so the run's outcome carries it. Why not, and nothing
    /// written, when `write` finds a sender cannot back it.
    async fn record(
        &self,
        task: &mut Context<'_, SendIntent, Progress>,
        progress: Progress,
        write: impl FnOnce(&mut SqliteConnection) -> anyhow::Result<()> + Send + 'static,
    ) -> anyhow::Result<Result<(), transaction::Refused>> {
        let job = task.job();
        let row = Row::try_from(&Job {
            state: Some(serde_json::to_value(&progress)?),
            ..job.clone()
        })?;
        let (id, attempt) = (int(job.id.0)?, i32::try_from(job.attempts)?);
        let now = i64::try_from(tasks::now())?;
        let recorded = self
            .db
            .write(move |connection| {
                // A refusal rolls the write back: nothing recorded.
                write(connection)?;
                ensure!(
                    task::db::update(connection, id, attempt, &row, now)?,
                    "the run has ended"
                );
                Ok(())
            })
            .await;
        if let Err(error) = recorded {
            return error.downcast::<transaction::Refused>().map(Err);
        }
        task.save(progress).await?;
        Ok(Ok(()))
    }

    /// Step 2: broadcasts recorded `txid` once; whether a node accepted it
    /// is the job's state (`sent`).
    async fn send(&self, network: &Network, txid: &str) -> anyhow::Result<Sent> {
        let Some(kept) = self.kept(network, txid).await? else {
            return Ok(Sent::Forgotten);
        };
        if kept.block.is_some() {
            return Ok(Sent::Mined);
        }
        Ok(
            match self
                .container
                .broadcast(network, &kept.transaction()?)
                .await
            {
                Ok(()) => Sent::Accepted,
                Err(error) if error.kind.retryable() => Sent::Unsure(error.message),
                Err(error) if error.kind == TransactionErrorKind::Rejected => {
                    Sent::Refused(refused(&error))
                }
                Err(error) => Sent::Refused(format!("invalid: {}", error.message)),
            },
        )
    }

    /// Step 3: where the payment of `progress` stands, by the transaction
    /// index of each version sent (085): any version may land. A version
    /// expired (Solana) by the height of the newest kept block at or below
    /// the confirmed position ([`TransactionIndex::expired`]); none kept
    /// there: none expired yet.
    async fn watch(
        &self,
        network: &Network,
        progress: &Progress,
        tracking: Tracking,
    ) -> anyhow::Result<Watched> {
        let ids = progress.ids(network)?;
        let at = network.clone();
        let (indexes, height) = self
            .db
            .read(move |connection| {
                let indexes = ids
                    .iter()
                    .map(|id| transaction_index::db::find(connection, id))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let confirmed = network_index::db::find_confirmed(connection, &at)?;
                let height = match confirmed {
                    Some(confirmed) => {
                        block::db::find_height_at_or_below(connection, &at, confirmed)?
                    }
                    None => None,
                };
                Ok((indexes, height))
            })
            .await?;
        let expired = |index: &TransactionIndex| {
            height
                .and_then(|height| u64::try_from(height).ok())
                .is_some_and(|height| index.expired(height))
        };
        let versions: Vec<&TransactionIndex> = indexes.iter().flatten().collect();
        for version in &versions {
            let txid = version.transaction.id.clone();
            match version.status() {
                TransactionStatus::Final => return Ok(Watched::Landed(txid)),
                TransactionStatus::Reverted => return Ok(Watched::Reverted),
                _ => {}
            }
        }
        let (expired, live): (Vec<&TransactionIndex>, Vec<&TransactionIndex>) =
            versions.into_iter().partition(|index| expired(index));
        let expired = expired
            .iter()
            .map(|index| TransactionId::new(network, &index.transaction.id))
            .collect::<Result<Vec<_>, _>>()?;
        let current = indexes.last().and_then(Option::as_ref);
        let current = current.filter(|current| live.contains(current));
        let current = match current {
            Some(current) if expired.is_empty() => current,
            None if expired.is_empty() && live.is_empty() => return Ok(Watched::Gone),
            _ => {
                let left = !live.is_empty();
                return Ok(Watched::Expired {
                    versions: expired,
                    left,
                });
            }
        };
        Ok(match current.status() {
            TransactionStatus::Final => Watched::Landed(progress.txid.clone()),
            TransactionStatus::Reverted => Watched::Reverted,
            TransactionStatus::GivenUp => Watched::Gone,
            TransactionStatus::Unknown => Watched::Unknown,
            // An earlier version found above the final block may land.
            TransactionStatus::Missing if live.iter().any(|index| index.found.is_some()) => {
                Watched::Unknown
            }
            // Bumps off (`max_bumps` 0): sent again as it is.
            _ if tracking.max_bumps > 0
                && progress.builder.is_some()
                && current.bump_due(tracking.bump_after) =>
            {
                Watched::BumpDue
            }
            TransactionStatus::Missing => Watched::Missing,
        })
    }

    /// Sends pending `txid` again; the answer decides nothing. `false` once
    /// it is no longer kept.
    async fn resend(&self, network: &Network, txid: &str) -> anyhow::Result<bool> {
        let Some(kept) = self.kept(network, txid).await? else {
            return Ok(false);
        };
        let _ignored = self
            .container
            .broadcast(network, &kept.transaction()?)
            .await;
        Ok(true)
    }

    /// Step 4: replaces the current version of `progress` with one at a
    /// higher fee ([`Self::replacement`]): a bump while bumps are left, else
    /// (or when a bump cannot be signed or funded) a cancel; a cancel's is a
    /// cancel again. Recorded in its place in one write
    /// ([`transaction_indexes::replace`], the job's state with it), then
    /// sent once. Refused by every node: undone in one write
    /// ([`transaction_indexes::restore`]), the old version current again
    /// as it was kept.
    async fn bump(
        &self,
        task: &mut Context<'_, SendIntent, Progress>,
        network: &Network,
        progress: &Progress,
        tracking: Tracking,
    ) -> anyhow::Result<Bumped> {
        let Some(state) = &progress.builder else {
            return Ok(Bumped::Stuck("no builder state to bump".into()));
        };
        let current = self
            .kept(network, &progress.txid)
            .await?
            .context("the current version is no longer kept")?;
        let mut cancel = !progress.cancels.is_empty() || progress.bumps >= tracking.max_bumps;
        let bumped = loop {
            let (next, builder) = match self.replacement(task.input(), state, cancel).await {
                Ok(next) => next,
                Err(error) if error.kind.retryable() => return Err(error.into()),
                // At the fee cap, short of change, or a chain that cannot
                // replace: cancelled instead.
                Err(_) if !cancel => {
                    cancel = true;
                    continue;
                }
                Err(error) => return Ok(Bumped::Stuck(error.message)),
            };
            let index = self.index(&next).await?;
            let txid = next.id.id.clone();
            let bumped = Progress {
                txid: txid.clone(),
                sent: true,
                replaced: [progress.replaced.clone(), vec![progress.txid.clone()]].concat(),
                bumps: progress.bumps + u32::from(!cancel),
                builder: Some(builder),
                cancels: [
                    progress.cancels.clone(),
                    cancel.then_some(txid).into_iter().collect(),
                ]
                .concat(),
                ..progress.clone()
            };
            let (old, mut pending) = (current.id.clone(), Transaction::try_from(&next)?);
            let replaced = self.record(task, bumped.clone(), move |connection| {
                transaction_indexes::replace(connection, &old, &mut pending, &index)
            });
            match replaced.await? {
                Ok(()) => break bumped,
                // Short of funds for the higher fee: a cancel pays only one.
                Err(_) if !cancel => cancel = true,
                Err(refused) => return Ok(Bumped::Stuck(refused.to_string())),
            }
        };
        let why = match self.send(network, &bumped.txid).await? {
            // Forgotten before it was sent: the next check finds it gone.
            Sent::Accepted | Sent::Mined | Sent::Forgotten => return Ok(Bumped::Sent(None)),
            Sent::Unsure(why) => return Ok(Bumped::Sent(Some(why))),
            Sent::Refused(why) => why,
        };
        let restored = Progress {
            bumps: bumped.bumps,
            ..progress.clone()
        };
        let (mut current, next) = (current, TransactionId::new(network, &bumped.txid)?);
        let undone = self.record(task, restored, move |connection| {
            transaction_indexes::restore(connection, &mut current, &next)
        });
        if let Err(refused) = undone.await? {
            anyhow::bail!("{why}; its undoing is refused: {refused}");
        }
        Ok(Bumped::Refused(why))
    }

    /// Drops `versions`, which can never land, in one write: each one's
    /// pending row with its legs (their funds free again) and its
    /// index; the job's state as `progress`, while the run holds it.
    async fn discard(
        &self,
        task: &mut Context<'_, SendIntent, Progress>,
        progress: &Progress,
        versions: Vec<TransactionId>,
    ) -> anyhow::Result<()> {
        if versions.is_empty() {
            return Ok(());
        }
        let dropped = self.record(task, progress.clone(), move |connection| {
            delete(connection, &versions)
        });
        if let Err(refused) = dropped.await? {
            anyhow::bail!("{refused}");
        }
        Ok(())
    }

    /// No version of `progress` left but `expired`: a new one signed by its
    /// builder on a fresh blockhash a tenth up and, in one write, `expired`
    /// dropped ([`delete`]) and the new one recorded as any (its own funds
    /// check, its index from the tip now), not sent yet: the job's state
    /// from then on. Short of funds: nothing written.
    async fn renew(
        &self,
        task: &mut Context<'_, SendIntent, Progress>,
        progress: &Progress,
        expired: Vec<TransactionId>,
    ) -> Result<(), Stop> {
        let Some(state) = &progress.builder else {
            return Err(Stop::Fail("expired".into()));
        };
        let (next, builder) = self
            .replacement(task.input(), state, false)
            .await
            .map_err(Stop::built)?;
        let index = self.index(&next).await?;
        let renewed = Progress {
            txid: next.id.id.clone(),
            sent: false,
            replaced: [progress.replaced.clone(), vec![progress.txid.clone()]].concat(),
            bumps: progress.bumps + 1,
            builder: Some(builder),
            ..progress.clone()
        };
        let mut pending = Transaction::try_from(&next)?;
        let recorded = self.record(task, renewed, move |connection| {
            delete(connection, &expired)?;
            transaction_indexes::record(connection, &mut pending, &index)
        });
        if let Err(refused) = recorded.await? {
            return Err(Stop::Fail(refused.to_string()));
        }
        Ok(())
    }

    /// The next version of the one whose builder saved `state`, signed by
    /// the kept wallet of `intent`'s sender at its fee level, the builder
    /// raising it at least its chain's step over its own, and its builder's
    /// state. Where the chain replaces ([`base::Replace`]): the same
    /// transfer in its slot (a bump), or zero to the sender when `cancel`;
    /// elsewhere (Solana) a new one after it expired, and no cancel.
    async fn replacement(
        &self,
        intent: &SendIntent,
        state: &Value,
        cancel: bool,
    ) -> Result<(base::Transaction, Value), TransactionError> {
        let wallet = self
            .container
            .wallet(&intent.sender())
            .await
            .map_err(TransactionError::unavailable)?;
        let mut builder = wallet.transaction(&intent.asset())?;
        builder.load(state)?;
        builder.with_fee(Fee::Set(intent.fee));
        let sender = intent.sender();
        match builder.replaceable() {
            Some(replace) if cancel => replace.replace(Transfer::new(sender, Decimal::zero()))?,
            Some(replace) => {
                let amount = intent
                    .amount
                    .parse::<Decimal>()
                    .map_err(|_| TransactionError::failed("invalid: amount is not a decimal"))?;
                let to = Address::new(sender.network, intent.to.clone());
                replace.replace(Transfer::new(to, amount))?;
            }
            None if cancel => {
                return Err(TransactionError::new(
                    TransactionErrorKind::Unsupported,
                    "this chain does not cancel a payment",
                ));
            }
            None => {}
        }
        let next = builder.sign().await?;
        Ok((next, builder.save()?))
    }

    /// Transaction `txid` of `network` as kept, pending or mined; `None`
    /// once forgotten.
    async fn kept(&self, network: &Network, txid: &str) -> anyhow::Result<Option<Transaction>> {
        let id = TransactionId::new(network, txid)?;
        self.db
            .read(move |connection| Ok(transaction::db::get(connection, &id)?))
            .await
    }

    /// Ends the payment of `progress` as `outcome` with `ack`, in one
    /// write: every version's pending row deleted with its legs (a
    /// mined one stays), every version's transaction index deleted, and the
    /// job's state with the outcome and `ack`'s reason. `ack`, once written.
    async fn end(
        &self,
        task: &mut Context<'_, SendIntent, Progress>,
        progress: Progress,
        outcome: Outcome,
        ack: Ack,
    ) -> Ack {
        let network = task.input().sender().network;
        let ids = match progress.ids(&network) {
            Ok(ids) => ids,
            Err(error) => return Ack::Retry(format!("{error:#}")),
        };
        let ended = Progress {
            outcome: Some(outcome),
            reason: match &ack {
                Ack::Fail(why) => Some(why.clone()),
                _ => None,
            },
            ..progress
        };
        let txid = ended.txid.clone();
        let written = self.record(task, ended, move |connection| delete(connection, &ids));
        match written.await {
            Ok(Ok(())) => {
                let name = serde_json::to_value(outcome).unwrap_or_default();
                let outcome = name.as_str().unwrap_or("-");
                if outcome == "final" {
                    tracing::info!(target: "send.end", txid, outcome, "Payment ended");
                } else {
                    tracing::warn!(target: "send.end", txid, outcome, "Payment ended");
                }
                ack
            }
            Ok(Err(refused)) => Ack::Retry(refused.to_string()),
            Err(error) => Ack::Retry(format!("{error:#}")),
        }
    }
}

impl Worker for Broadcast {
    const KIND: &'static str = KIND;
    /// Four runs at once, each waiting its turn on its sender.
    const POLICY: Policy = Policy {
        concurrency: NonZeroUsize::new(4).unwrap(),
        timeout: TIMEOUT,
        ..Policy::DEFAULT
    };
    type Input = SendIntent;
    type State = Progress;

    /// `chain:network.address`: its sender on its network.
    fn topic(intent: &SendIntent) -> Option<Topic> {
        Topic::new([
            format!("{}:{}", intent.chain, intent.network),
            intent.address.clone(),
        ])
    }

    /// One run, in its network's span (inside the executor's job span):
    /// see the module.
    async fn run(&self, task: Context<'_, SendIntent, Progress>) -> Ack {
        let span = crate::usecase::network_span(&task.input().sender().network);
        self.payment(task).instrument(span).await
    }
}

impl Broadcast {
    /// [`Worker::run`]'s work.
    async fn payment(&self, mut task: Context<'_, SendIntent, Progress>) -> Ack {
        let network = task.input().sender().network;
        let tracking = self.tracking(&network);
        let retry = |error: anyhow::Error| Ack::Retry(format!("{error:#}"));
        let again = |why: Option<String>| Ack::Again(next_check(tracking.check), why);

        // No node to call: waits, never fails for it.
        if !self.container.has_node(&network) {
            return again(Some(NO_ENABLED_ENDPOINT.to_owned()));
        }

        // 1. Build: sign, record and watch.
        let mut progress = match task.state().cloned() {
            Some(progress) => progress,
            None => match self.build(&mut task).await {
                Ok(progress) => {
                    let txid = &progress.txid;
                    tracing::info!(target: "send.sign", txid, "Payment signed");
                    progress
                }
                Err(Stop::Retry(why)) => {
                    tracing::warn!(target: "send.sign", why, "Payment not signed; retried");
                    return Ack::Retry(why);
                }
                Err(Stop::Fail(why)) => {
                    tracing::warn!(target: "send.sign", why, "Payment not signed; failed");
                    return Ack::Fail(why);
                }
            },
        };

        // 2. Send, once.
        if !progress.sent {
            let sent = self.send(&network, &progress.txid).await;
            let outcome = match &sent {
                Ok(Sent::Accepted) => "accepted",
                Ok(Sent::Unsure(_)) => "unsure",
                Ok(Sent::Mined) => "mined",
                Ok(Sent::Refused(_)) => "refused",
                Ok(Sent::Forgotten) => "forgotten",
                Err(_) => "retry",
            };
            let txid = &progress.txid;
            if matches!(sent, Ok(Sent::Accepted | Sent::Mined)) {
                tracing::info!(target: "send.broadcast", txid, outcome, "Payment broadcast");
            } else {
                tracing::warn!(target: "send.broadcast", txid, outcome, "Payment broadcast");
            }
            let why = match sent {
                Ok(Sent::Accepted | Sent::Mined) => None,
                Ok(Sent::Unsure(why)) => Some(why),
                Ok(Sent::Refused(why)) => {
                    return self
                        .end(&mut task, progress, Outcome::Refused, Ack::Fail(why))
                        .await;
                }
                Ok(Sent::Forgotten) => {
                    let ack = Ack::Fail("forgotten".into());
                    return self.end(&mut task, progress, Outcome::Forgotten, ack).await;
                }
                Err(error) => return retry(error),
            };
            progress.sent = true;
            if let Err(error) = task.save(progress).await {
                return Ack::Retry(error.to_string());
            }
            return again(why);
        }

        // 3. Watch it resolve: any version sent.
        let (outcome, ack) = match self.watch(&network, &progress, tracking).await {
            Ok(Watched::Landed(txid)) if progress.cancels.contains(&txid) => {
                (Outcome::Cancelled, Ack::Fail("cancelled".into()))
            }
            Ok(Watched::Landed(_)) => (Outcome::Final, Ack::Done),
            // Mined, as when landed; the money did not move.
            Ok(Watched::Reverted) => (Outcome::Reverted, Ack::Fail("reverted".into())),
            // Another version left: the expired ones dropped, it watched on.
            Ok(Watched::Expired {
                versions,
                left: true,
            }) => {
                let dropped = self.discard(&mut task, &progress, versions).await;
                return again(dropped.err().map(|error| format!("{error:#}")));
            }
            // None left, no bump: the end drops every version.
            Ok(Watched::Expired { .. }) if progress.bumps >= tracking.max_bumps => {
                (Outcome::Expired, Ack::Fail("expired".into()))
            }
            // The new version is sent by the next run, at once.
            Ok(Watched::Expired { versions, .. }) => {
                match self.renew(&mut task, &progress, versions).await {
                    Ok(()) => return Ack::Again(tasks::now(), None),
                    Err(Stop::Retry(why)) => return again(Some(why)),
                    Err(Stop::Fail(why)) => (Outcome::Expired, Ack::Fail(why)),
                }
            }
            Ok(Watched::Gone) => (Outcome::Forgotten, Ack::Fail("forgotten".into())),
            Ok(Watched::Unknown) => return again(None),
            Ok(Watched::Missing) => match self.resend(&network, &progress.txid).await {
                Ok(true) => return again(None),
                Ok(false) => (Outcome::Forgotten, Ack::Fail("forgotten".into())),
                Err(error) => return again(Some(format!("{error:#}"))),
            },
            // 4. Bump: a replacement at a higher fee in its place, or a
            //    cancel; none (stuck): the same bytes again.
            Ok(Watched::BumpDue) => {
                let bumped = self.bump(&mut task, &network, &progress, tracking).await;
                let outcome = match &bumped {
                    Ok(Bumped::Sent(_)) => "sent",
                    Ok(Bumped::Refused(_)) => "refused",
                    Ok(Bumped::Stuck(_)) => "stuck",
                    Err(_) => "retry",
                };
                let now = task.state().unwrap_or(&progress);
                let txid = now.txid.clone();
                let cancel = now.cancels.contains(&txid);
                match (cancel, matches!(bumped, Ok(Bumped::Sent(_)))) {
                    (false, true) => {
                        tracing::info!(target: "send.bump", txid, outcome, "Payment bumped")
                    }
                    (false, false) => {
                        tracing::warn!(target: "send.bump", txid, outcome, "Payment bumped")
                    }
                    (true, true) => {
                        tracing::info!(target: "send.cancel", txid, outcome, "Payment cancelled")
                    }
                    (true, false) => {
                        tracing::warn!(target: "send.cancel", txid, outcome, "Payment cancelled")
                    }
                }
                let why = match bumped {
                    Ok(Bumped::Sent(why)) => why,
                    Ok(Bumped::Refused(why)) => Some(why),
                    Ok(Bumped::Stuck(why)) => {
                        let _sent = self.resend(&network, &progress.txid).await;
                        Some(format!("stuck: {why}"))
                    }
                    Err(error) => Some(format!("{error:#}")),
                };
                return again(why);
            }
            Err(error) => return again(Some(format!("{error:#}"))),
        };
        self.end(&mut task, progress, outcome, ack).await
    }
}

/// Deletes versions `ids` in the caller's write: each one's pending row
/// with its legs (the `transaction` model; a mined one stays) and its
/// index (the `transaction_index` model).
fn delete(connection: &mut SqliteConnection, ids: &[TransactionId]) -> anyhow::Result<()> {
    transaction::db::delete_pending(connection, ids)?;
    for id in ids {
        transaction_index::db::delete(connection, id)?;
    }
    Ok(())
}

/// When the next check is due, `check` from now.
fn next_check(check: Duration) -> u64 {
    tasks::now().saturating_add(millis(check))
}

/// A broadcast's recorded transaction (txid), once recorded, and whether it
/// was sent; `None` and `false` for other kinds.
pub(crate) fn progress(state: Option<&Value>) -> (Option<String>, bool) {
    let Some(progress) = state
        .cloned()
        .and_then(|state| serde_json::from_value::<Progress>(state).ok())
    else {
        return (None, false);
    };
    (Some(progress.txid), progress.sent)
}

/// A refusal as a job's reason: `refused: {reason}: {node's text}`, e.g.
/// `refused: nonce-used: nonce too low`; a reason in the chain's own words
/// is the text already.
fn refused(error: &TransactionError) -> String {
    let named = match &error.reason {
        Some(Reason::InsufficientFunds) => "insufficient-funds",
        Some(Reason::NonceUsed) => "nonce-used",
        Some(Reason::FeeTooLow) => "fee-too-low",
        Some(Reason::Expired) => "expired",
        Some(Reason::Invalid) => "invalid",
        Some(Reason::Reverted) => "reverted",
        Some(Reason::Other(_)) | None => return format!("refused: {}", error.message),
    };
    format!("refused: {named}: {}", error.message)
}

#[cfg(test)]
mod tests {
    use base::{Reason, TransactionError, TransactionErrorKind};

    use super::refused;

    #[test]
    fn a_refusal_names_its_reason_and_the_nodes_text() {
        let error = |reason: Option<Reason>| {
            let error = TransactionError::new(TransactionErrorKind::Rejected, "nonce too low");
            refused(&match reason {
                Some(reason) => error.with_reason(reason),
                None => error,
            })
        };
        assert_eq!(
            error(Some(Reason::NonceUsed)),
            "refused: nonce-used: nonce too low"
        );
        assert_eq!(
            error(Some(Reason::Other("code -32002".into()))),
            "refused: nonce too low"
        );
        assert_eq!(error(None), "refused: nonce too low");
    }
}
