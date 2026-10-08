//! A scripted wallet double for tests of code that holds [`crate::Wallets`]:
//! one [`Factory`], the wallets it makes, and the [`Outbox`] their sends
//! reach, with every send recorded and its outcome scripted.
//!
//! It speaks no chain. An address is `addr-` followed by any text, in any
//! case, and its canonical form is lowercase. A wallet's address is `addr-`
//! and the first byte of its secret in hexadecimal; generation always makes
//! the secret `[7; 32]`, so every generated wallet is `addr-07`. Membership is
//! proved the way a chain proves it ([`crate::Origin`]): a wallet belongs to a
//! factory when it is this double's wallet, of the same network, over the same
//! outbox.
//!
//! Behind the `testing` feature, which is off by default and belongs in a
//! `[dev-dependencies]` entry only.

use std::{
    collections::VecDeque,
    future::poll_fn,
    sync::{Arc, Mutex, MutexGuard},
    task::{Poll, Waker},
};

use base::Holder;
use base::{
    Address, AssetId, Broadcaster, Network, SignFuture, SignRequest, Signer, SignerError,
    SignerErrorKind, TransactionBuilder, TransactionError, TransactionErrorKind, TransactionId,
    TransactionSnapshot,
};

use crate::{
    AddressParser, Amount, Author, Authorship, Dependency, FactoryProfile, FutureResult,
    Generation, Origin, SecretBytes, SendError, SendFuture, Sender, SingleSender,
    TransactionFactory, Transfer, Wallet, WalletError, WalletErrorKind, WalletFactory,
};

/// The text every address of the double starts with.
const PREFIX: &str = "addr-";

/// How the outbox answers every send it records.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Outcome {
    /// Accepted: a single send answers `single`, a batch answers `[batch]`.
    #[default]
    Accepted,
    /// Refused with this kind before anything was accepted.
    Refused(WalletErrorKind),
    /// Unknown: nothing is definitely accepted and the local transaction id
    /// `ambiguous` is reported for reconciliation.
    Ambiguous,
    /// A batch whose first transaction, `accepted`, was accepted before a
    /// failure of this kind; a single send is refused with it.
    Partial(WalletErrorKind),
}

/// One send that reached the double, with its destinations and amounts in
/// authored order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Submission {
    /// One wallet's own send ([`SingleSender::send`]).
    Single(Address, Amount),
    /// One batch through the asset's [`Sender`].
    Batch(Vec<(Address, Amount)>),
}

#[derive(Default)]
struct Record {
    outcome: Outcome,
    /// Answers for the next sends, one each, before `outcome` applies again.
    script: VecDeque<Outcome>,
    submissions: Vec<Submission>,
    held: bool,
    waiting: Vec<Waker>,
    sending: usize,
    peak: usize,
}

/// The send pipeline of one [`Factory`]'s asset: it records every
/// [`Submission`] and answers each with the scripted [`Outcome`]. Clones share
/// one record.
#[derive(Clone, Default)]
pub struct Outbox(Arc<Mutex<Record>>);

impl Outbox {
    /// Answers every later send with `outcome`.
    pub fn answer(&self, outcome: Outcome) {
        self.record().outcome = outcome;
    }

    /// Answers the next sends with `outcomes`, one each and in order, before
    /// the standing answer applies again.
    pub fn script(&self, outcomes: impl IntoIterator<Item = Outcome>) {
        self.record().script.extend(outcomes);
    }

    /// Keeps every send that reaches the double in progress until
    /// [`Self::release`].
    pub fn hold(&self) {
        self.record().held = true;
    }

    /// Lets every held send finish, and later ones finish at once.
    pub fn release(&self) {
        let waiting = {
            let mut record = self.record();
            record.held = false;
            std::mem::take(&mut record.waiting)
        };
        for waker in waiting {
            waker.wake();
        }
    }

    /// The most sends ever in progress at the same time.
    #[must_use]
    pub fn peak(&self) -> usize {
        self.record().peak
    }

    /// Every send that reached the double so far, in order; empty when
    /// nothing was sent.
    #[must_use]
    pub fn submissions(&self) -> Vec<Submission> {
        self.record().submissions.clone()
    }

    fn record(&self) -> MutexGuard<'_, Record> {
        // A test that panicked while holding the lock already failed; its
        // record is still the truth about what was sent.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records `submission`, keeps it in progress while the outbox is held,
    /// and answers with the scripted outcome.
    async fn submit(&self, submission: Submission) -> Outcome {
        let outcome = {
            let mut record = self.record();
            record.submissions.push(submission);
            record.sending += 1;
            record.peak = record.peak.max(record.sending);
            record
                .script
                .pop_front()
                .unwrap_or_else(|| record.outcome.clone())
        };
        poll_fn(|context| {
            let mut record = self.record();
            if record.held {
                record.waiting.push(context.waker().clone());
                return Poll::Pending;
            }
            record.sending -= 1;
            Poll::Ready(())
        })
        .await;
        outcome
    }

    fn refusal(kind: WalletErrorKind) -> WalletError {
        WalletError::new(kind, "the wallet double refused this send")
    }

    fn ambiguity() -> WalletError {
        TransactionError::new(
            TransactionErrorKind::Timeout,
            "the wallet double cannot tell whether this send was accepted",
        )
        .with_ambiguous_transaction_id(TransactionId::new("ambiguous"))
        .into()
    }
}

impl Sender for Outbox {
    fn send<'a>(&'a self, transfers: Vec<Transfer>) -> SendFuture<'a> {
        let sent = transfers
            .into_iter()
            .map(|transfer| (transfer.to, transfer.amount))
            .collect();
        Box::pin(async move {
            match self.submit(Submission::Batch(sent)).await {
                Outcome::Accepted => Ok(vec![TransactionId::new("batch")]),
                Outcome::Refused(kind) => Err(SendError::grouped(Vec::new(), Self::refusal(kind))),
                Outcome::Ambiguous => Err(SendError::grouped(Vec::new(), Self::ambiguity())),
                Outcome::Partial(kind) => Err(SendError::item(
                    1,
                    vec![TransactionId::new("accepted")],
                    Self::refusal(kind),
                )),
            }
        })
    }
}

impl Authorship for Outbox {
    fn author(&self, wallet: &Arc<dyn Wallet>) -> Result<Arc<dyn Author>, WalletError> {
        let account = Account::of(wallet.as_ref())?;
        Ok(Arc::new(account.clone()))
    }
}

/// A wallet factory of one network whose asset is that chain's native one,
/// or the token [`Factory::token`] names.
///
/// It spends its asset through its [`Outbox`] unless built
/// [`Factory::receiving`], and generates keys unless built
/// [`Factory::failing`].
pub struct Factory {
    network: Network,
    asset: AssetId,
    origin: Origin<(Network, Dependency)>,
    outbox: Outbox,
    generation: Option<WalletErrorKind>,
    spends: bool,
}

impl Factory {
    /// A factory of `network` over a new outbox that accepts every send.
    #[must_use]
    pub fn new(network: Network) -> Self {
        let outbox = Outbox::default();
        Self {
            origin: Origin::new((network.clone(), Dependency::of(&outbox.0))),
            asset: AssetId::native(network.chain.clone()),
            network,
            outbox,
            generation: None,
            spends: true,
        }
    }

    /// The same factory, with every key generation failing as `kind`.
    #[must_use]
    pub fn failing(mut self, kind: WalletErrorKind) -> Self {
        self.generation = Some(kind);
        self
    }

    /// The same factory, declaring an asset the system receives and never
    /// spends: it has no [`Authorship`].
    #[must_use]
    pub fn receiving(mut self) -> Self {
        self.spends = false;
        self
    }

    /// The same factory, holding the token `asset` of its chain instead of
    /// the chain's native asset.
    #[must_use]
    pub fn token(mut self, asset: impl Into<String>) -> Self {
        self.asset = AssetId {
            chain: self.network.chain.clone(),
            asset: asset.into(),
        };
        self
    }

    /// The outbox this factory's sends reach.
    #[must_use]
    pub fn outbox(&self) -> Outbox {
        self.outbox.clone()
    }
}

impl WalletFactory for Factory {
    fn create<'a>(&'a self, secret: SecretBytes) -> FutureResult<'a, Arc<dyn Wallet>> {
        let first = secret.as_bytes().first().copied().unwrap_or_default();
        let account = Account {
            address: Address::new(self.network.clone(), format!("{PREFIX}{first:02x}")),
            asset: self.asset(),
            outbox: self.outbox.clone(),
        };
        Box::pin(async move { Ok(Arc::new(account) as Arc<dyn Wallet>) })
    }

    fn generate(&self) -> FutureResult<'_, Generation> {
        let secret = match self.generation {
            Some(kind) => Err(WalletError::new(
                kind,
                "the wallet double failed to generate",
            )),
            None => Ok(SecretBytes::new([7; 32])),
        };
        Generation::of(self, secret)
    }

    fn validate(&self, wallet: &dyn Wallet) -> Result<(), WalletError> {
        self.origin.validate(wallet, |account: &Account| {
            (
                account.address.network.clone(),
                Dependency::of(&account.outbox.0),
            )
        })
    }
}

impl AddressParser for Factory {
    fn parse_address(&self, text: &str) -> Result<Address, WalletError> {
        let canonical = text.to_lowercase();
        if canonical.len() <= PREFIX.len() || !canonical.starts_with(PREFIX) {
            return Err(WalletError::new(
                WalletErrorKind::InvalidAddress,
                "a wallet double address is `addr-` and more text",
            ));
        }
        Ok(Address::new(self.network.clone(), canonical))
    }
}

impl FactoryProfile for Factory {
    fn network(&self) -> &Network {
        &self.network
    }

    fn asset(&self) -> AssetId {
        self.asset.clone()
    }

    fn key_format(&self) -> base::KeyFormat {
        base::KeyFormat::Secp256k1Scalar
    }

    fn authorship(&self) -> Option<Arc<dyn Authorship>> {
        self.spends
            .then(|| Arc::new(self.outbox.clone()) as Arc<dyn Authorship>)
    }
}

/// One wallet of a [`Factory`]: an address, its asset, and the outbox its
/// sends reach. It signs nothing and builds no transaction.
#[derive(Clone)]
struct Account {
    address: Address,
    asset: AssetId,
    outbox: Outbox,
}

impl Account {
    fn of(wallet: &dyn Wallet) -> Result<&Self, WalletError> {
        (wallet as &dyn std::any::Any)
            .downcast_ref::<Self>()
            .ok_or_else(|| {
                WalletError::new(
                    WalletErrorKind::Unsupported,
                    "not a wallet of the wallet double",
                )
            })
    }
}

impl Holder for Account {
    fn address(&self) -> &Address {
        &self.address
    }

    fn asset(&self) -> &AssetId {
        &self.asset
    }
}

impl Signer for Account {
    fn sign<'a>(&'a self, _request: SignRequest) -> SignFuture<'a> {
        Box::pin(async {
            Err(SignerError {
                kind: SignerErrorKind::UnsupportedOperation,
                message: "the wallet double signs nothing".to_owned(),
            })
        })
    }
}

impl SingleSender for Account {
    fn send<'a>(&'a self, destination: Address, amount: Amount) -> FutureResult<'a, TransactionId> {
        Box::pin(async move {
            match self
                .outbox
                .submit(Submission::Single(destination, amount))
                .await
            {
                Outcome::Accepted => Ok(TransactionId::new("single")),
                Outcome::Refused(kind) | Outcome::Partial(kind) => Err(Outbox::refusal(kind)),
                Outcome::Ambiguous => Err(Outbox::ambiguity()),
            }
        })
    }
}

/// The double sends through its outbox only; nothing in it drafts, restores
/// or broadcasts a transaction.
impl TransactionFactory for Account {
    /// # Panics
    ///
    /// Always: the double builds no transaction.
    fn transaction(&self) -> Box<dyn TransactionBuilder> {
        panic!("the wallet double builds no transaction")
    }

    /// # Panics
    ///
    /// Always: the double restores no transaction.
    fn resume(
        &self,
        _snapshot: &TransactionSnapshot,
    ) -> Result<Box<dyn TransactionBuilder>, TransactionError> {
        panic!("the wallet double restores no transaction")
    }

    /// # Panics
    ///
    /// Always: the double broadcasts nothing.
    fn broadcaster(&self) -> &dyn Broadcaster {
        panic!("the wallet double broadcasts nothing")
    }
}
