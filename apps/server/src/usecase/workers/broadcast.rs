//! The broadcast job (designs 066, 084, 085): one per [`SendIntent`], one
//! payment, one transaction, owned until it lands in a final block or can
//! never land. It locks its sender on its network (`chain:network.address`),
//! so a sender's runs go one at a time, and a trigger round (the whole
//! network) never runs beside one. Several payments are several jobs.
//!
//! Each run takes the next step its state calls for:
//!
//! 1. **Build** (no state yet): sign the payment, then, in one write, check
//!    the sender holds it beyond what is in flight, record it as pending,
//!    watch it with its transaction index (085), and save the job's state.
//!    Short of funds: nothing is recorded, the job fails.
//! 2. **Send** (recorded, not sent): broadcast it once. Refused by every
//!    node: it is forgotten (`refused`), the job fails. Accepted, or no
//!    sure answer (a node may hold it): marked sent; checked every
//!    [`CHECK`] from then on.
//! 3. **Watch** (sent): read its transaction index, which the indexer moves
//!    with every block it keeps.
//!    - found in a final block: done;
//!    - can never land (Solana, read past its expiry): forgotten (`dead`),
//!      failed;
//!    - given up or gone (a jump): failed `forgotten`;
//!    - in none of the blocks read since it was sent: sent again (a stuck
//!      one's fee bump, 084, goes here);
//!    - not known yet (found above the final block, or not read that far):
//!      checked again.
//!
//! When the job ends, its transaction index goes, and a transaction still
//! pending is forgotten. A rerun never builds again. Between checks a job
//! blocks nobody, so the sender's next payment runs; it never gives up on
//! its payment.

use std::{num::NonZeroUsize, sync::Arc, time::Duration};

use anyhow::ensure;
use base::{
    Address, Asset, Chain, Decimal, IntoIndex as _, Network, TransactionErrorKind,
    TransactionIndex, TransactionIndexTarget, TransactionStatus, Transfer, WalletError,
};
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

/// How long a sent payment waits for its next check.
pub(crate) const CHECK: Duration = Duration::from_secs(60);

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
}

/// How far a job got: its state, once its transaction is recorded.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Progress {
    /// The recorded transaction's id (txid).
    pub(crate) txid: String,
    /// Whether it was sent: a node accepted it, or may hold it.
    pub(crate) sent: bool,
}

impl SendIntent {
    /// The sending wallet's address.
    pub(crate) fn sender(&self) -> Address {
        let network = Network::new(Chain::new(&self.chain), &self.network);
        Address::new(network, &self.address)
    }
}

/// The broadcast worker: the container that signs and sends, the database
/// that records, and how long a sent payment waits for its next check.
pub(crate) struct Broadcast {
    container: Arc<base::Container<crate::usecase::Transport>>,
    db: Db,
    check: Duration,
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
    fn built(error: base::TransactionError) -> Self {
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

impl Broadcast {
    /// Checks each sent payment every `check` ([`CHECK`] but in tests).
    pub(crate) const fn new(
        container: Arc<base::Container<crate::usecase::Transport>>,
        db: Db,
        check: Duration,
    ) -> Self {
        Self {
            container,
            db,
            check,
        }
    }

    /// Step 1: signs the payment and, in one write, records it watched by
    /// its transaction index: the job's state from then on.
    async fn build(&self, task: &mut Context<'_, SendIntent, Progress>) -> Result<Progress, Stop> {
        let signed = self.sign(task.input()).await?;
        let index = self.index(&signed).await?;
        let progress = Progress {
            txid: signed.id.id.clone(),
            sent: false,
        };
        if !self.record(task.job(), &signed, &index, &progress).await? {
            return Err(Stop::Fail("insufficient-funds".into()));
        }
        // Saved again, so the run's outcome carries it too.
        task.save(progress.clone())
            .await
            .map_err(|error| Stop::Retry(error.to_string()))?;
        Ok(progress)
    }

    /// The payment of `intent`, signed by the kept wallet of its sender.
    async fn sign(&self, intent: &SendIntent) -> Result<base::Transaction, Stop> {
        let sender = intent.sender();
        let wallet = self.container.wallet(&sender).await.map_err(|error| {
            match error.downcast_ref::<WalletError>() {
                Some(wallet) if !wallet.kind.retryable() => {
                    Stop::Fail(format!("invalid: {}", wallet.message))
                }
                _ => Stop::from(error),
            }
        })?;
        let asset = Asset {
            network: sender.network.clone(),
            id: intent.asset.clone(),
        };
        let amount = intent
            .amount
            .parse::<Decimal>()
            .map_err(|_| Stop::Fail("invalid: amount is not a decimal".into()))?;
        let to = Address::new(sender.network, intent.to.clone());
        let builder = wallet.transaction(&asset).map_err(Stop::built)?;
        builder
            .sign(Transfer::new(to, amount))
            .await
            .map_err(Stop::built)
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

    /// In one write: checks `signed` against what its senders hold beyond
    /// what is in flight, records it as pending, not yet sent, with `index`,
    /// and stores `job` with `progress` as its state, while the run still
    /// holds it. `false`, and nothing written, when a sender is short.
    async fn record(
        &self,
        job: &Job,
        signed: &base::Transaction,
        index: &TransactionIndex,
        progress: &Progress,
    ) -> anyhow::Result<bool> {
        let mut pending = Transaction::try_from(signed)?;
        let index = index.clone();
        let row = Row::try_from(&Job {
            state: Some(serde_json::to_value(progress)?),
            ..job.clone()
        })?;
        let (id, attempt) = (int(job.id.0)?, i32::try_from(job.attempts)?);
        let now = i64::try_from(tasks::now())?;
        let recorded = self
            .db
            .write(move |connection| {
                // Short rolls the write back: nothing recorded.
                transaction::db::save_pending(connection, &mut pending)?;
                transaction_index::db::insert(connection, &index)?;
                ensure!(
                    task::db::update(connection, id, attempt, &row, now)?,
                    "the run has ended"
                );
                Ok(())
            })
            .await;
        match recorded {
            Ok(()) => Ok(true),
            Err(error) if error.is::<transaction::Short>() => Ok(false),
            Err(error) => Err(error),
        }
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
                return Ok(Sent::Refused(format!("refused: {}", error.message)));
            }
            Err(error) => return Ok(Sent::Refused(format!("invalid: {}", error.message))),
        };
        let now = i64::try_from(tasks::now())?;
        self.db
            .write(move |connection| Ok(transaction::db::set_sent(connection, &id, now)?))
            .await?;
        Ok(sent)
    }

    /// Step 3: where sent `txid` stands, by its transaction index (085);
    /// `None` once it is no longer watched (a jump forgot it).
    async fn status(
        &self,
        network: &Network,
        txid: &str,
    ) -> anyhow::Result<Option<TransactionStatus>> {
        let id = TransactionId::new(network, txid)?;
        let index = self
            .db
            .read(move |connection| transaction_index::db::find(connection, &id))
            .await?;
        Ok(index.map(|index| index.status()))
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

    /// Transaction `txid` of `network` as kept, pending or mined; `None`
    /// once forgotten.
    async fn kept(&self, network: &Network, txid: &str) -> anyhow::Result<Option<Transaction>> {
        let id = TransactionId::new(network, txid)?;
        self.db
            .read(move |connection| Ok(transaction::db::get(connection, &id)?))
            .await
    }

    /// Ends payment `txid` in one write: its transaction index deleted, and
    /// its transaction forgotten for `why` while still pending.
    async fn end(&self, network: &Network, txid: &str, why: Why) -> anyhow::Result<()> {
        let id = TransactionId::new(network, txid)?;
        self.db
            .write(move |connection| {
                transaction_indexes::forget(connection, &[&id], why)?;
                transaction_index::db::delete(connection, &id)?;
                Ok(())
            })
            .await
    }

    /// When the next check is due.
    fn next_check(&self) -> u64 {
        tasks::now().saturating_add(millis(self.check))
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
        let retry = |error: anyhow::Error| Ack::Retry(format!("{error:#}"));
        let again = |why: Option<String>| Ack::Again(self.next_check(), why);

        // 1. Build: sign, record and watch.
        let mut progress = match task.state().cloned() {
            Some(progress) => progress,
            None => match self.build(&mut task).await {
                Ok(progress) => progress,
                Err(Stop::Retry(why)) => return Ack::Retry(why),
                Err(Stop::Fail(why)) => return Ack::Fail(why),
            },
        };
        let txid = progress.txid.clone();

        // 2. Send, once.
        if !progress.sent {
            let why = match self.send(&network, &txid).await {
                Ok(Sent::Accepted | Sent::Mined) => None,
                Ok(Sent::Unsure(why)) => Some(why),
                Ok(Sent::Refused(why)) => {
                    let ended = self.end(&network, &txid, Why::Refused).await;
                    return ended.map_or_else(retry, |()| Ack::Fail(why));
                }
                Ok(Sent::Forgotten) => {
                    let ended = self.end(&network, &txid, Why::Jump).await;
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

        // 3. Watch it land.
        let status = match self.status(&network, &txid).await {
            Ok(status) => status,
            Err(error) => return again(Some(format!("{error:#}"))),
        };
        let (why, ack) = match status {
            // Still pending here only when found inside a gap never read.
            Some(TransactionStatus::Final) => (Why::Jump, Ack::Done),
            Some(TransactionStatus::Dead) => (Why::Dead, Ack::Fail("dead".into())),
            None | Some(TransactionStatus::GivenUp) => (Why::Jump, Ack::Fail("forgotten".into())),
            Some(TransactionStatus::Unknown) => return again(None),
            // A stuck one's fee bump (084) goes here, before the send again.
            Some(TransactionStatus::Missing) => match self.resend(&network, &txid).await {
                Ok(true) => return again(None),
                Ok(false) => (Why::Jump, Ack::Fail("forgotten".into())),
                Err(error) => return again(Some(format!("{error:#}"))),
            },
        };
        let ended = self.end(&network, &txid, why).await;
        ended.map_or_else(retry, |()| ack)
    }
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
