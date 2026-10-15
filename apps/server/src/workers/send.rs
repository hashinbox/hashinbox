//! The send queue: one job per [`SendIntent`], one lane per sender, so a
//! sender's sends run one at a time, in order.
//!
//! The job only sends — build, save, broadcast — and never tracks mining:
//!
//! 1. **Resume.** Signed bytes in the job's payload are reused; never rebuilt.
//! 2. **Build and sign** every transaction of the intent up front: the kept
//!    wallet's `transaction(asset)`, its transfers, `sign()`; the chain's
//!    builder fetches its own inputs.
//! 3. **State first.** The signed bytes are saved into the job; a retry
//!    resends exactly those bytes. Nothing is written to the index: indexing
//!    alone records the transactions once they are seen in blocks.
//! 4. **Broadcast** each, in order, through `Payments::broadcast`: the
//!    accepted count is saved (Ethereum's accepted prefix); a refusal fails
//!    the job; an expired Solana blockhash fails it as `outcome-unknown`;
//!    anything transient retries with the same bytes.

use std::{future::Future, pin::Pin, sync::Arc};

use base::{
    Address, Asset, BuilderExt as _, Chain, Decimal, Network, TransactionErrorKind, WalletError,
};
use payments::Payments;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tasks::{Ack, Executor, Job, Store};

/// The job kind.
pub(crate) const KIND: &str = "send";

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

/// How far a job got, saved in its payload beside the intent.
#[derive(Default, Serialize, Deserialize)]
struct Progress {
    /// Every transaction's exact signed bytes in hex, in broadcast order.
    #[serde(default)]
    signed: Vec<String>,
    /// How many of `signed`, a prefix, the nodes accepted.
    #[serde(default)]
    accepted: usize,
}

impl SendIntent {
    /// The sending wallet's address.
    pub(crate) fn sender(&self) -> Address {
        let network = Network::new(Chain::new(&self.chain), &self.network);
        Address::new(network, &self.address)
    }

    /// The job of this intent, on its sender's lane.
    pub(crate) fn job(&self) -> Job {
        let lane = format!("{}:{}:{}", self.chain, self.network, self.address);
        Job::new(lane, KIND, json!({ "intent": self }))
    }
}

/// The send executor: the payments container that signs and broadcasts.
#[derive(Clone)]
pub(crate) struct Sends {
    payments: Arc<Payments>,
}

impl Sends {
    pub(crate) const fn new(payments: Arc<Payments>) -> Self {
        Self { payments }
    }

    /// One execution of `job`.
    async fn run(&self, mut job: Job, store: Arc<dyn Store>) -> Ack {
        let Some(intent) = job
            .payload
            .get("intent")
            .cloned()
            .and_then(|intent| serde_json::from_value::<SendIntent>(intent).ok())
        else {
            return Ack::Fail("invalid-input".into());
        };
        let sender = intent.sender();
        let mut progress = job
            .payload
            .get("progress")
            .cloned()
            .and_then(|progress| serde_json::from_value::<Progress>(progress).ok())
            .unwrap_or_default();
        if progress.signed.is_empty() {
            progress.signed = match self.sign(&sender, &intent).await {
                Ok(signed) => signed,
                Err(ack) => return ack,
            };
            // State first: nothing is broadcast without its job's bytes.
            if let Err(ack) = Self::save(&mut job, &progress, store.as_ref()).await {
                return ack;
            }
        }
        while let Some(saved) = progress.signed.get(progress.accepted) {
            let Ok(bytes) = hex::decode(saved) else {
                return Ack::Fail("invalid: saved bytes are not hex".into());
            };
            match self.payments.broadcast(&sender.network, &bytes).await {
                Ok(()) => {
                    progress.accepted += 1;
                    if let Err(ack) = Self::save(&mut job, &progress, store.as_ref()).await {
                        return ack;
                    }
                }
                Err(error) if error.kind == TransactionErrorKind::Rejected => {
                    return Ack::Fail(format!("refused: {}", error.message));
                }
                // The transaction may still land; indexing is the authority.
                Err(error) if error.kind == TransactionErrorKind::Expired => {
                    return Ack::Fail("outcome-unknown".into());
                }
                Err(error) if error.kind.retryable() => return Ack::Retry(error.message),
                Err(error) => return Ack::Fail(format!("invalid: {}", error.message)),
            }
        }
        Ack::Done
    }

    /// Every transaction of `intent`, signed by the kept wallet of `sender`,
    /// as hex. A missing key fails the job; a store failure retries.
    async fn sign(&self, sender: &Address, intent: &SendIntent) -> Result<Vec<String>, Ack> {
        let wallet = self.payments.wallet(sender).await.map_err(|error| {
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
        let signed = builder.sign().await.map_err(Self::refused)?;
        Ok(signed
            .iter()
            .map(|transaction| hex::encode(&transaction.bytes))
            .collect())
    }

    /// A build failure: transient ones retry, the rest fail the job.
    fn refused(error: base::TransactionError) -> Ack {
        if error.kind.retryable() {
            Ack::Retry(error.message)
        } else {
            Ack::Fail(format!("invalid: {}", error.message))
        }
    }

    /// Saves `progress` into the running job.
    async fn save(job: &mut Job, progress: &Progress, store: &dyn Store) -> Result<(), Ack> {
        let value = serde_json::to_value(progress).map_err(|_| Ack::Fail("unsaveable".into()))?;
        if let Some(payload) = job.payload.as_object_mut() {
            payload.insert("progress".into(), value);
        }
        store
            .set(job.clone())
            .await
            .map_err(|error| Ack::Retry(error.to_string()))
    }
}

impl Executor for Sends {
    fn execute(
        &self,
        job: Job,
        store: Arc<dyn Store>,
    ) -> Pin<Box<dyn Future<Output = Ack> + Send>> {
        let this = self.clone();
        Box::pin(async move { this.run(job, store).await })
    }
}

/// How many transactions a job's payload records as signed, and how many
/// of them, a prefix, the nodes accepted.
pub(crate) fn progress(payload: &Value) -> (usize, usize) {
    let progress = payload
        .get("progress")
        .cloned()
        .and_then(|progress| serde_json::from_value::<Progress>(progress).ok())
        .unwrap_or_default();
    (progress.signed.len(), progress.accepted)
}
