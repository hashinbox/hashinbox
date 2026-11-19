//! The broadcast job (designs 066, 084, 085): one per [`SendIntent`], one
//! payment, owned until a version of it lands in a final block or it can
//! never land. It locks its sender on its network (`chain:network.address`),
//! so a sender's runs go one at a time, and a trigger round (the whole
//! network) never runs beside one. Several payments are several jobs.
//!
//! Each run takes the next step its state calls for:
//!
//! 1. **Build** (no state yet): sign the payment at its fee level, then, in
//!    one write, check the sender's indexes are kept and hold it beyond
//!    what is in flight, record it as pending, watch it with its transaction
//!    index (085), and save the job's state. Short of funds, or a sender
//!    index missing: nothing is recorded, the job fails with the reason. A
//!    paused one still sends: its transaction index watches it all the same.
//! 2. **Send** (recorded, not sent): broadcast it once. Refused by every
//!    node: it is forgotten (`refused`), the job fails. Accepted, or no
//!    sure answer (a node may hold it): marked sent; checked every
//!    [`Tracking::check`] of its chain from then on.
//! 3. **Watch** (sent): read the transaction index of each version sent,
//!    which the indexer moves with every block it keeps.
//!    - a version found in a final block: done (the current one forgotten
//!      `replaced` when another landed); reverted there: failed `reverted`
//!      (only its fee paid), kept the same way;
//!    - the current one can never land (Solana, read past its expiry):
//!      bumped while [`Tracking::max_bumps`] allows, else forgotten
//!      (`dead`), failed;
//!    - its index given up or gone (a jump): failed `forgotten`;
//!    - in none of the blocks read since it was sent: sent again, or, once
//!      its index read [`Tracking::bump_after`] blocks past its broadcast
//!      (never one that expires: it cannot be replaced in flight), bumped;
//!    - not known yet (a version found above the final block, or not read
//!      that far): checked again.
//! 4. **Bump** (due): a replacement at the job's fee level and at least the
//!    chain's step above the current fee, recorded in its place in one write
//!    (the current one forgotten `replaced`, its index watching on; the new
//!    one pending, its funds checked, with its own index; the job's state),
//!    then sent once. Refused by every node: undone, the old one current
//!    again. At [`Tracking::max_bumps`], at the fee cap, short of change,
//!    or on a chain that cannot replace: the same bytes again, the job's
//!    reason `stuck`. A job queued before its builder's state was kept is
//!    never bumped, only watched.
//!
//! When the job ends, the index of each version goes, and the current one,
//! still pending, is forgotten. A rerun never builds again. Between checks
//! a job blocks nobody, so the sender's next payment runs; it never gives
//! up on its payment.

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

use super::TIMEOUT;
use crate::{
    db::Db,
    models::{
        Task as Row, int, task, transaction,
        transaction::{Transaction, TransactionId},
        transaction_forgotten::Why,
        transaction_index,
    },
    usecase::sdk::transaction_indexes,
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
    /// This version (txid) is in a final block, reverted: only its fee paid.
    Reverted(String),
    /// The current version can never land (Solana: read past its expiry),
    /// and no bump is left.
    Dead,
    /// The current version's index was given up (a jump), or is gone.
    Gone,
    /// Not known yet: a version found above the final block, or not read
    /// that far.
    Unknown,
    /// The current version is in none of the blocks read since it was sent,
    /// through the final one.
    Missing,
    /// Due a bump ([`TransactionIndex::bump_due`]): missing, its index read
    /// `bump_after` blocks past its broadcast; or one that expires, dead
    /// with bumps left.
    BumpDue,
}

/// What a bump came to.
enum Bumped {
    /// The replacement is current and sent; why not surely (a node's doubt).
    Sent(Option<String>),
    /// Every node refused the replacement (why): undone, the old version
    /// current again.
    Refused(String),
    /// No replacement (why): `max_bumps` made, the fee cap, short of funds,
    /// or a chain that cannot replace.
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

    /// Step 2: broadcasts recorded `txid` once and, unless every node
    /// refused it, marks it sent.
    async fn send(&self, network: &Network, txid: &str) -> anyhow::Result<Sent> {
        let Some(kept) = self.kept(network, txid).await? else {
            return Ok(Sent::Forgotten);
        };
        if kept.block.is_some() {
            return Ok(Sent::Mined);
        }
        let id = kept.id.clone();
        let sent = match self
            .container
            .broadcast(network, &kept.transaction()?)
            .await
        {
            Ok(()) => Sent::Accepted,
            Err(error) if error.kind.retryable() => Sent::Unsure(error.message),
            Err(error) if error.kind == TransactionErrorKind::Rejected => {
                return Ok(Sent::Refused(refused(&error)));
            }
            Err(error) => return Ok(Sent::Refused(format!("invalid: {}", error.message))),
        };
        let now = i64::try_from(tasks::now())?;
        self.db
            .write(move |connection| Ok(transaction::db::set_sent(connection, &id, now)?))
            .await?;
        Ok(sent)
    }

    /// Step 3: where the payment of `progress` stands, by the transaction
    /// index of each version sent (085): any version may land.
    async fn watch(
        &self,
        network: &Network,
        progress: &Progress,
        tracking: Tracking,
    ) -> anyhow::Result<Watched> {
        let ids = progress.ids(network)?;
        let indexes = self
            .db
            .read(move |connection| {
                ids.iter()
                    .map(|id| transaction_index::db::find(connection, id))
                    .collect::<anyhow::Result<Vec<_>>>()
            })
            .await?;
        let versions: Vec<&TransactionIndex> = indexes.iter().flatten().collect();
        for version in &versions {
            let txid = version.transaction.id.clone();
            match version.status() {
                TransactionStatus::Final => return Ok(Watched::Landed(txid)),
                TransactionStatus::Reverted => return Ok(Watched::Reverted(txid)),
                _ => {}
            }
        }
        let Some(Some(current)) = indexes.last() else {
            return Ok(Watched::Gone);
        };
        Ok(match current.status() {
            TransactionStatus::Final => Watched::Landed(progress.txid.clone()),
            TransactionStatus::Reverted => Watched::Reverted(progress.txid.clone()),
            TransactionStatus::GivenUp => Watched::Gone,
            TransactionStatus::Unknown => Watched::Unknown,
            // An earlier version found above the final block may land.
            TransactionStatus::Missing if versions.iter().any(|index| index.found.is_some()) => {
                Watched::Unknown
            }
            // One that expires is not sent again once out of bumps.
            TransactionStatus::Dead if progress.bumps >= tracking.max_bumps => Watched::Dead,
            // Bumps off (`max_bumps` 0): sent again as it is.
            _ if tracking.max_bumps > 0
                && progress.builder.is_some()
                && current.bump_due(tracking.bump_after) =>
            {
                Watched::BumpDue
            }
            TransactionStatus::Dead => Watched::Dead,
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
    /// higher fee ([`Self::replacement`]): recorded in its place in one write
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
        if progress.bumps >= tracking.max_bumps {
            return Ok(Bumped::Stuck(format!("{} bumps made", progress.bumps)));
        }
        let Some(state) = &progress.builder else {
            return Ok(Bumped::Stuck("no builder state to bump".into()));
        };
        let current = self
            .kept(network, &progress.txid)
            .await?
            .context("the current version is no longer kept")?;
        let (next, builder) = match self.replacement(task.input(), state).await {
            Ok(next) => next,
            Err(error) if error.kind.retryable() => return Err(error.into()),
            // At the fee cap, or a chain that cannot replace.
            Err(error) => return Ok(Bumped::Stuck(error.message)),
        };
        let index = self.index(&next).await?;
        let bumped = Progress {
            txid: next.id.id.clone(),
            sent: true,
            replaced: [progress.replaced.clone(), vec![progress.txid.clone()]].concat(),
            bumps: progress.bumps + 1,
            builder: Some(builder),
        };
        let (old, mut pending) = (current.id.clone(), Transaction::try_from(&next)?);
        let replaced = self.record(task, bumped.clone(), move |connection| {
            transaction_indexes::replace(connection, &old, &mut pending, &index)
        });
        if let Err(refused) = replaced.await? {
            return Ok(Bumped::Stuck(refused.to_string()));
        }
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

    /// The replacement of the version whose builder saved `state`, signed by
    /// the kept wallet of `intent`'s sender at a tenth above the higher of
    /// the rate now and its own, and its builder's state.
    async fn replacement(
        &self,
        intent: &SendIntent,
        state: &Value,
    ) -> Result<(base::Transaction, Value), TransactionError> {
        let wallet = self
            .container
            .wallet(&intent.sender())
            .await
            .map_err(TransactionError::unavailable)?;
        let mut builder = wallet.transaction(&intent.asset())?;
        builder.load(state)?;
        builder.with_fee(Fee::Times(1.1));
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

    /// Ends the payment of `progress` in one write: the transaction index of
    /// each version deleted, and the current one forgotten for `why` while
    /// still pending.
    async fn end(&self, network: &Network, progress: &Progress, why: Why) -> anyhow::Result<()> {
        let (current, ids) = (
            TransactionId::new(network, &progress.txid)?,
            progress.ids(network)?,
        );
        self.db
            .write(move |connection| {
                transaction_indexes::forget(connection, &[&current], why)?;
                for id in &ids {
                    transaction_index::db::delete(connection, id)?;
                }
                Ok(())
            })
            .await
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

    async fn run(&self, mut task: Context<'_, SendIntent, Progress>) -> Ack {
        let network = task.input().sender().network;
        let tracking = self.tracking(&network);
        let retry = |error: anyhow::Error| Ack::Retry(format!("{error:#}"));
        let again = |why: Option<String>| Ack::Again(next_check(tracking.check), why);

        // 1. Build: sign, record and watch.
        let mut progress = match task.state().cloned() {
            Some(progress) => progress,
            None => match self.build(&mut task).await {
                Ok(progress) => progress,
                Err(Stop::Retry(why)) => return Ack::Retry(why),
                Err(Stop::Fail(why)) => return Ack::Fail(why),
            },
        };

        // 2. Send, once.
        if !progress.sent {
            let why = match self.send(&network, &progress.txid).await {
                Ok(Sent::Accepted | Sent::Mined) => None,
                Ok(Sent::Unsure(why)) => Some(why),
                Ok(Sent::Refused(why)) => {
                    let ended = self.end(&network, &progress, Why::Refused).await;
                    return ended.map_or_else(retry, |()| Ack::Fail(why));
                }
                Ok(Sent::Forgotten) => {
                    let ended = self.end(&network, &progress, Why::Jump).await;
                    return ended.map_or_else(retry, |()| Ack::Fail("forgotten".into()));
                }
                Err(error) => return retry(error),
            };
            progress.sent = true;
            if let Err(error) = task.save(progress).await {
                return Ack::Retry(error.to_string());
            }
            return again(why);
        }

        // 3. Watch it land: any version sent.
        let (why, ack) = match self.watch(&network, &progress, tracking).await {
            // Still pending here only when found inside a gap never read.
            Ok(Watched::Landed(txid)) if txid == progress.txid => (Why::Jump, Ack::Done),
            // An earlier version landed: the current one never will.
            Ok(Watched::Landed(_)) => (Why::Replaced, Ack::Done),
            // Mined, as when landed; the money did not move.
            Ok(Watched::Reverted(txid)) if txid == progress.txid => {
                (Why::Jump, Ack::Fail("reverted".into()))
            }
            Ok(Watched::Reverted(_)) => (Why::Replaced, Ack::Fail("reverted".into())),
            Ok(Watched::Dead) => (Why::Dead, Ack::Fail("dead".into())),
            Ok(Watched::Gone) => (Why::Jump, Ack::Fail("forgotten".into())),
            Ok(Watched::Unknown) => return again(None),
            Ok(Watched::Missing) => match self.resend(&network, &progress.txid).await {
                Ok(true) => return again(None),
                Ok(false) => (Why::Jump, Ack::Fail("forgotten".into())),
                Err(error) => return again(Some(format!("{error:#}"))),
            },
            // 4. Bump: a replacement at a higher fee in its place; none
            //    (stuck): the same bytes again.
            Ok(Watched::BumpDue) => {
                let why = match self.bump(&mut task, &network, &progress, tracking).await {
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
        let ended = self.end(&network, &progress, why).await;
        ended.map_or_else(retry, |()| ack)
    }
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
