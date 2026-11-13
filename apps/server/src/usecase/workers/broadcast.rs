//! The broadcast job (design 066): one per [`SendIntent`], locking its sender
//! on its network (`chain:network.address`), so a sender's sends run one at a
//! time and a trigger round (the whole network) never runs beside one.
//!
//! 1. **Build and sign** every transaction of the intent: the kept wallet's
//!    `transaction(asset)`, its transfers, `sign()`; the chain's builder
//!    fetches its own inputs (coins spent by pending transactions excluded).
//! 2. **Check and record, in one write:** every (address, asset) a
//!    transaction takes from must hold it on top of what is in flight; then
//!    each is recorded as pending and the job's state saved with their ids.
//!    Short of funds: nothing is recorded, the job fails.
//! 3. **Broadcast** each recorded transaction, in order, through the
//!    network's own broadcaster; the sent count is saved after each. A
//!    transient failure retries with the same bytes; anything else forgets
//!    what is not sent and fails.
//!
//! A rerun with saved state never builds again: it broadcasts the recorded
//! transactions not yet sent; one forgotten meanwhile (expired) fails it.

use std::{num::NonZeroUsize, sync::Arc};

use anyhow::ensure;
use base::{
    Address, Asset, BuilderExt as _, Chain, Decimal, Network, TransactionErrorKind, WalletError,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tasks::{Ack, Context, Job, Policy, Topic, Worker};

use super::TIMEOUT;
use crate::{
    db::Db,
    models::{
        Task as Row, int, task, transaction,
        transaction::{Transaction, TransactionId},
    },
};

/// The job kind.
pub(crate) const KIND: &str = "broadcast";

/// What a caller enqueues: an ordered list of transfers of one asset from
/// one kept wallet, and nothing else.
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
    /// The transfers, in order.
    pub(crate) transfers: Vec<Payment>,
}

/// One transfer of a [`SendIntent`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Payment {
    /// The canonical recipient address in the sender's network.
    pub(crate) to: String,
    /// Exact atomic units, canonical decimal text.
    pub(crate) amount: String,
}

/// How far a job got: its state.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Progress {
    /// Every recorded transaction's id (txid), in broadcast order.
    #[serde(default)]
    pub(crate) ids: Vec<String>,
    /// How many of `ids`, a prefix, the nodes accepted.
    #[serde(default)]
    pub(crate) sent: usize,
}

impl SendIntent {
    /// The sending wallet's address.
    pub(crate) fn sender(&self) -> Address {
        let network = Network::new(Chain::new(&self.chain), &self.network);
        Address::new(network, &self.address)
    }
}

/// The broadcast worker: the container that signs and sends, and
/// the database that records.
pub(crate) struct Broadcast {
    container: Arc<base::Container<crate::usecase::Transport>>,
    db: Db,
}

impl Broadcast {
    pub(crate) const fn new(
        container: Arc<base::Container<crate::usecase::Transport>>,
        db: Db,
    ) -> Self {
        Self { container, db }
    }

    /// Every transaction of `intent`, signed by the kept wallet of `sender`.
    /// A missing key fails the job; a store failure retries.
    async fn sign(&self, sender: &Address, intent: &SendIntent) -> Result<base::Transactions, Ack> {
        let wallet = self.container.wallet(sender).await.map_err(|error| {
            match error.downcast_ref::<WalletError>() {
                Some(wallet) if !wallet.kind.retryable() => {
                    Ack::Fail(format!("invalid: {}", wallet.message))
                }
                _ => Ack::Retry(format!("{error:#}")),
            }
        })?;
        let asset = Asset {
            network: sender.network.clone(),
            id: intent.asset.clone(),
        };
        let mut builder = wallet.transaction(&asset).map_err(Self::refused)?;
        for payment in &intent.transfers {
            let amount = payment
                .amount
                .parse::<Decimal>()
                .map_err(|_| Ack::Fail("invalid: amount is not a decimal".into()))?;
            let to = Address::new(sender.network.clone(), payment.to.clone());
            builder.send((to, amount)).map_err(Self::refused)?;
        }
        builder.sign().await.map_err(Self::refused)
    }

    /// A build failure: transient ones retry, the rest fail the job.
    fn refused(error: base::TransactionError) -> Ack {
        if error.kind.retryable() {
            Ack::Retry(error.message)
        } else {
            Ack::Fail(format!("invalid: {}", error.message))
        }
    }

    /// In one write: checks every transaction of `signed` against what its
    /// senders hold beyond what is in flight, records each as pending and
    /// stores `job` with their ids as its state, while the run still holds
    /// it. `false`, and nothing written, when a sender is short.
    async fn record(&self, job: &Job, signed: base::Transactions) -> anyhow::Result<bool> {
        let ids: Vec<String> = signed
            .iter()
            .map(|transaction| transaction.id.id.clone())
            .collect();
        let state = serde_json::to_value(Progress { ids, sent: 0 })?;
        let row = Row::try_from(&Job {
            state: Some(state),
            ..job.clone()
        })?;
        let (id, attempt) = (int(job.id.0)?, i32::try_from(job.attempts)?);
        let now = i64::try_from(tasks::now())?;
        let recorded = self
            .db
            .write(move |connection| {
                // A short one rolls the write back: nothing recorded.
                for signed in signed.iter() {
                    let mut pending = Transaction::try_from(signed)?;
                    pending.sent_at = Some(now);
                    transaction::db::save_pending(connection, &mut pending)?;
                }
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

    /// The pending transaction `txid` of `network`, if it is still pending.
    async fn pending(
        &self,
        network: &Network,
        txid: &str,
    ) -> anyhow::Result<Option<base::Transaction>> {
        let id = TransactionId::new(network, txid)?;
        self.db
            .read(move |connection| {
                transaction::db::get(connection, &id)?
                    .filter(|kept| kept.block.is_none())
                    .map(Transaction::transaction)
                    .transpose()
            })
            .await
    }

    /// Forgets the pending transactions `txids` of `network`: never sent, or
    /// refused.
    async fn forget(&self, network: &Network, txids: &[String]) -> anyhow::Result<()> {
        let ids = txids
            .iter()
            .map(|txid| TransactionId::new(network, txid))
            .collect::<Result<Vec<_>, _>>()?;
        self.db
            .write(move |connection| Ok(transaction::db::delete_pending(connection, &ids)?))
            .await?;
        Ok(())
    }
}

impl Worker for Broadcast {
    const KIND: &'static str = KIND;
    /// Four sends at once, each waiting its turn on its sender.
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
        let mut progress = match task.state() {
            Some(progress) => progress.clone(),
            None => {
                let sender = task.input().sender();
                let signed = match self.sign(&sender, task.input()).await {
                    Ok(signed) => signed,
                    Err(ack) => return ack,
                };
                let ids = signed
                    .iter()
                    .map(|transaction| transaction.id.id.clone())
                    .collect();
                match self.record(task.job(), signed).await {
                    // Saved again, so the run's outcome carries it too.
                    Ok(true) => {
                        let progress = Progress { ids, sent: 0 };
                        if let Err(error) = task.save(progress.clone()).await {
                            return Ack::Retry(error.to_string());
                        }
                        progress
                    }
                    Ok(false) => return Ack::Fail("insufficient-funds".into()),
                    Err(error) => return Ack::Retry(format!("{error:#}")),
                }
            }
        };
        while let Some(txid) = progress.ids.get(progress.sent).cloned() {
            let transaction = match self.pending(&network, &txid).await {
                Ok(Some(transaction)) => transaction,
                // Forgotten meanwhile (expired): it may never land.
                Ok(None) => return Ack::Fail("outcome-unknown".into()),
                Err(error) => return Ack::Retry(format!("{error:#}")),
            };
            match self.container.broadcast(&network, &transaction).await {
                Ok(()) => {
                    progress.sent += 1;
                    if let Err(error) = task.save(progress.clone()).await {
                        return Ack::Retry(error.to_string());
                    }
                }
                // It may have landed: the record stays; the same bytes go again.
                Err(error) if error.kind.retryable() => return Ack::Retry(error.message),
                // Refused or expired: it and the rest will not be sent.
                Err(error) => {
                    let unsent = &progress.ids[progress.sent..];
                    if let Err(forgot) = self.forget(&network, unsent).await {
                        return Ack::Retry(format!("{forgot:#}"));
                    }
                    let refused = error.kind == TransactionErrorKind::Rejected;
                    let prefix = if refused { "refused" } else { "invalid" };
                    return Ack::Fail(format!("{prefix}: {}", error.message));
                }
            }
        }
        Ack::Done
    }
}

/// How many transactions a job's state records as signed, and how many of
/// them, a prefix, the nodes accepted.
pub(crate) fn progress(state: Option<&Value>) -> (usize, usize) {
    let progress = state
        .cloned()
        .and_then(|state| serde_json::from_value::<Progress>(state).ok())
        .unwrap_or_default();
    (progress.ids.len(), progress.sent)
}
