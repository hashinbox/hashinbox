//! A send queue: a transparent [`Authorship`] decorator that admits sends,
//! dispatches them first in, first out with a bounded number in flight, and
//! retries only what is honestly retryable.
//!
//! It wraps an asset's authorship, so both of the asset's send paths pass
//! through it: a batch ([`Sender::send`]) and one wallet's own send
//! ([`crate::SingleSender::send`] on the [`crate::Author`] it hands out).
//! A caller cannot tell it is there: the answer is the inner sender's, later.
//!
//! **Nothing runs in the background.** Every send is its own future, waiting
//! its turn at two fair gates ([`Limit`]): admission, sized by the queue's
//! capacity, and dispatch, sized by how many sends may be with the node at
//! once. The future is the backpressure. The crate still names no runtime:
//! the pause between retries is the caller's ([`Pause`]).
//!
//! **A retry never risks a double send.** Only a failure that accepted nothing,
//! is not ambiguous, and is of a transient kind ([`WalletErrorKind::retryable`])
//! goes back to the end of the line. An accepted prefix or an ambiguous
//! outcome is answered at once: the chain's own recovery owns those.
//!
//! **In memory only.** A send not yet admitted never existed; an admitted one
//! is answered while the process lives. Durable delivery is not this type's.

use std::{
    collections::VecDeque,
    future::Future,
    num::{NonZeroU32, NonZeroUsize},
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
    time::Duration,
};

use base::Holder;
use base::{Address, AssetId, Broadcaster, SignFuture, SignRequest, Signer, TransactionBuilder};

use crate::{
    Amount, Author, Authorship, FutureResult, SendError, SendFuture, Sender, SingleSender,
    TransactionFactory, Transfer, Wallet, WalletError, WalletErrorKind, pause::Gate,
};

/// Waits `delay`. The queue sleeps only through this, so the runtime is the
/// caller's: an application on Tokio passes `tokio::time::sleep`.
pub type Pause = Arc<dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// The wait before retry `attempt` (the first retry is attempt 1): doubling
/// from `initial`, capped at `max`, then jittered down by up to half so
/// retries that failed together do not return together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    /// The base wait before the first retry.
    pub initial: Duration,
    /// The longest base wait.
    pub max: Duration,
}

impl Backoff {
    fn delay(self, attempt: u32, seed: u64) -> Duration {
        let doubled = 1_u32
            .checked_shl(attempt.saturating_sub(1))
            .and_then(|factor| self.initial.checked_mul(factor))
            .unwrap_or(self.max)
            .min(self.max);
        // A deterministic spread in [half, whole] from the send's sequence:
        // enough to scatter a burst, and reproducible in a test.
        let spread = seed
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(u64::from(attempt))
            % 1001;
        let half = doubled / 2;
        let spread = (doubled - half).as_nanos() * u128::from(spread) / 1000;
        half + Duration::from_nanos(u64::try_from(spread).unwrap_or(u64::MAX))
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(250),
            max: Duration::from_secs(5),
        }
    }
}

/// Everything about a [`Queue`] that is tuned rather than invariant.
#[derive(Clone)]
pub struct Policy {
    /// How many sends may be with the inner sender at once.
    pub in_flight: NonZeroUsize,
    /// How many sends may be admitted and not yet answered; a send beyond it
    /// waits for room.
    pub capacity: NonZeroUsize,
    /// How many times a retryable failure is tried in all, the first included.
    pub attempts: NonZeroU32,
    /// The wait between tries.
    pub backoff: Backoff,
    /// How the queue waits; the default does not wait at all.
    pub pause: Pause,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            in_flight: NonZeroUsize::new(4).unwrap_or(NonZeroUsize::MIN),
            capacity: NonZeroUsize::new(256).unwrap_or(NonZeroUsize::MIN),
            attempts: NonZeroU32::new(3).unwrap_or(NonZeroU32::MIN),
            backoff: Backoff::default(),
            pause: Arc::new(|_| Box::pin(async {})),
        }
    }
}

/// What a queue is doing now, read without touching its dispatch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Admitted and not yet answered.
    pub depth: usize,
    /// With the inner sender right now.
    pub in_flight: usize,
    /// Answered since the queue was made, success or failure.
    pub completed: u64,
}

/// An asset's [`Authorship`] behind a send queue. See the module docs.
pub struct Queue<S> {
    inner: S,
    lane: Arc<Lane>,
}

impl<S> Queue<S> {
    /// `inner` behind a queue of its own, `policy.in_flight` sends at a time.
    #[must_use]
    pub fn new(inner: S, policy: Policy) -> Self {
        let limit = Arc::new(Limit::new(policy.in_flight));
        Self::sharing(inner, policy, limit)
    }

    /// `inner` behind a queue whose in-flight slots are `limit`'s, shared with
    /// every other queue holding it — the queues of all assets on one node.
    #[must_use]
    pub fn sharing(inner: S, policy: Policy, limit: Arc<Limit>) -> Self {
        Self::gated(inner, policy, limit, None)
    }

    /// [`Self::sharing`], each send also admitted by its network's `gate`
    /// ([`crate::Wallets::pause`]).
    pub(crate) fn gated(
        inner: S,
        policy: Policy,
        limit: Arc<Limit>,
        gate: Option<Arc<Gate>>,
    ) -> Self {
        Self {
            inner,
            lane: Arc::new(Lane {
                admission: Arc::new(Limit::new(policy.capacity)),
                dispatch: limit,
                gate,
                policy,
                closed: AtomicBool::new(false),
                depth: AtomicUsize::new(0),
                in_flight: AtomicUsize::new(0),
                completed: AtomicU64::new(0),
                sequence: AtomicU64::new(0),
                drained: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Refuses every later send with [`WalletErrorKind::Unavailable`]; sends
    /// already admitted are still dispatched and answered.
    pub fn close(&self) {
        self.lane.closed.store(true, Ordering::Release);
    }

    /// Resolves once nothing admitted is left unanswered.
    pub fn drained(&self) -> impl Future<Output = ()> + Send + '_ {
        Drain(&self.lane)
    }

    /// What the queue is doing now.
    #[must_use]
    pub fn stats(&self) -> Stats {
        Stats {
            depth: self.lane.depth.load(Ordering::Acquire),
            in_flight: self.lane.in_flight.load(Ordering::Acquire),
            completed: self.lane.completed.load(Ordering::Acquire),
        }
    }
}

impl<S: Sender> Sender for Queue<S> {
    fn send<'a>(&'a self, transfers: Vec<Transfer>) -> SendFuture<'a> {
        Box::pin(self.lane.run(
            move || self.inner.send(transfers.iter().map(Lane::copy).collect()),
            |error: &SendError| {
                error.accepted.is_empty()
                    && error.ambiguous_transaction_id.is_none()
                    && error.source.kind.retryable()
            },
            |refusal: WalletError| SendError::definite(refusal.kind, refusal.message),
        ))
    }
}

impl<S: Authorship> Authorship for Queue<S> {
    fn author(&self, wallet: &Arc<dyn Wallet>) -> Result<Arc<dyn Author>, WalletError> {
        let inner = self.inner.author(wallet)?;
        Ok(Arc::new(QueuedAuthor::new(inner, self.lane.clone())))
    }
}

/// The state every send of one queue shares.
struct Lane {
    admission: Arc<Limit>,
    dispatch: Arc<Limit>,
    /// The network's pause, when [`crate::Wallets::queue`] made this lane.
    gate: Option<Arc<Gate>>,
    policy: Policy,
    closed: AtomicBool,
    depth: AtomicUsize,
    in_flight: AtomicUsize,
    completed: AtomicU64,
    sequence: AtomicU64,
    /// Whoever waits for [`Queue::drained`].
    drained: Mutex<Vec<Waker>>,
}

impl Lane {
    const CLOSED: &'static str = "the send queue is closed";

    /// One send through the queue: admitted, dispatched in turn, and retried
    /// while `retryable` says the failure changed nothing. A closed queue or
    /// a paused network answers `refused` of why.
    async fn run<T, E, F, A>(
        &self,
        mut attempt: A,
        retryable: impl Fn(&E) -> bool,
        refused: impl Fn(WalletError) -> E,
    ) -> Result<T, E>
    where
        A: FnMut() -> F,
        F: Future<Output = Result<T, E>>,
    {
        let closed = || refused(WalletError::new(WalletErrorKind::Unavailable, Self::CLOSED));
        if self.closed.load(Ordering::Acquire) {
            return Err(closed());
        }
        // Counted from here until answered, so a pause waits for this send.
        let _unpaused = self
            .gate
            .as_ref()
            .map(Gate::admit)
            .transpose()
            .map_err(&refused)?;
        let _admitted = self.admission.acquire().await;
        // Waiting for room is not admission: a queue closed meanwhile refuses.
        if self.closed.load(Ordering::Acquire) {
            return Err(closed());
        }
        let _depth = Count::up(self, &self.depth);
        let seed = self.sequence.fetch_add(1, Ordering::Relaxed);
        let mut tries = 1;
        loop {
            let result = {
                let _slot = self.dispatch.acquire().await;
                let _flying = Count::up(self, &self.in_flight);
                attempt().await
            };
            match result {
                Err(error) if retryable(&error) && tries < self.policy.attempts.get() => {
                    (self.policy.pause)(self.policy.backoff.delay(tries, seed)).await;
                    tries += 1;
                }
                answered => {
                    self.completed.fetch_add(1, Ordering::AcqRel);
                    return answered;
                }
            }
        }
    }

    /// A transfer again, for another try: the batch is handed over by value.
    fn copy(transfer: &Transfer) -> Transfer {
        Transfer {
            wallet: transfer.wallet.clone(),
            to: transfer.to.clone(),
            amount: transfer.amount.clone(),
        }
    }

    fn settle(&self) {
        let wakers =
            std::mem::take(&mut *self.drained.lock().unwrap_or_else(PoisonError::into_inner));
        for waker in wakers {
            waker.wake();
        }
    }
}

/// One step of a counter, taken back when dropped — also when a send is
/// cancelled mid-way.
struct Count<'a> {
    lane: &'a Lane,
    counter: &'a AtomicUsize,
}

impl<'a> Count<'a> {
    fn up(lane: &'a Lane, counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self { lane, counter }
    }
}

impl Drop for Count<'_> {
    fn drop(&mut self) {
        let left = self.counter.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 && std::ptr::eq(self.counter, &self.lane.depth) {
            self.lane.settle();
        }
    }
}

/// The wait for every admitted send to be answered.
struct Drain<'a>(&'a Lane);

impl Future for Drain<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        let mut waiting = self
            .0
            .drained
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.0.depth.load(Ordering::Acquire) == 0 {
            return Poll::Ready(());
        }
        waiting.push(context.waker().clone());
        Poll::Pending
    }
}

// The fair gate behind admission and dispatch.

/// At most a fixed number of holders at once, admitted first come, first
/// served.
///
/// One `Limit` can be shared by several [`crate::Queue`]s: the queues of
/// every asset served by one node hold one limit, so together they never put
/// more than its slots in flight against that node.
pub struct Limit(Mutex<Slots>);

struct Slots {
    free: usize,
    next: u64,
    /// Tickets in arrival order, each with the waker of the future holding it.
    waiting: VecDeque<(u64, Option<Waker>)>,
}

impl Limit {
    /// A gate of `slots` simultaneous holders.
    #[must_use]
    pub fn new(slots: NonZeroUsize) -> Self {
        Self(Mutex::new(Slots {
            free: slots.get(),
            next: 0,
            waiting: VecDeque::new(),
        }))
    }

    /// Waits for a slot behind everyone who asked earlier.
    fn acquire(self: &Arc<Self>) -> Ticket {
        let mut slots = self.slots();
        let ticket = slots.next;
        slots.next = slots.next.wrapping_add(1);
        slots.waiting.push_back((ticket, None));
        Ticket {
            limit: self.clone(),
            ticket: Some(ticket),
        }
    }

    // Every critical section here is one queue edit or one counter step, so a
    // panic cannot leave the slots torn: recovering the guard is sound.
    fn slots(&self) -> MutexGuard<'_, Slots> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The waker of whoever is first in line now, to be woken outside the lock.
    fn front(slots: &mut Slots) -> Option<Waker> {
        slots
            .waiting
            .front_mut()
            .and_then(|(_, waker)| waker.take())
    }

    fn release(&self) {
        let waker = {
            let mut slots = self.slots();
            slots.free += 1;
            Self::front(&mut slots)
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// A place in line for one slot of a [`Limit`]. Dropping it gives the place up.
struct Ticket {
    limit: Arc<Limit>,
    ticket: Option<u64>,
}

impl Future for Ticket {
    type Output = Permit;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Permit> {
        let Some(ticket) = self.ticket else {
            panic!("an acquired slot was polled again");
        };
        let mut slots = self.limit.slots();
        let first = slots.waiting.front().is_some_and(|(at, _)| *at == ticket);
        if first && slots.free > 0 {
            slots.free -= 1;
            slots.waiting.pop_front();
            // The next in line may fit in a slot that is still free.
            let next = (slots.free > 0).then(|| Limit::front(&mut slots)).flatten();
            drop(slots);
            if let Some(waker) = next {
                waker.wake();
            }
            self.ticket = None;
            return Poll::Ready(Permit(self.limit.clone()));
        }
        if let Some((_, waker)) = slots.waiting.iter_mut().find(|(at, _)| *at == ticket) {
            *waker = Some(context.waker().clone());
        }
        Poll::Pending
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        let Some(ticket) = self.ticket else {
            return;
        };
        let waker = {
            let mut slots = self.limit.slots();
            let was_first = slots.waiting.front().is_some_and(|(at, _)| *at == ticket);
            slots.waiting.retain(|(at, _)| *at != ticket);
            was_first.then(|| Limit::front(&mut slots)).flatten()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// One held slot of a [`Limit`], given back when dropped.
struct Permit(Arc<Limit>);

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.release();
    }
}

// One wallet's author behind its asset's queue: its own send waits in the
// same line as the asset's batches; everything else is the inner author's.

/// An [`Author`] whose [`SingleSender::send`] is queued.
///
/// Drafting, restoring and broadcasting a transaction by hand
/// ([`TransactionFactory`]) is not a send the queue can see, and passes
/// straight through.
struct QueuedAuthor {
    inner: Arc<dyn Author>,
    lane: Arc<Lane>,
}

impl QueuedAuthor {
    fn new(inner: Arc<dyn Author>, lane: Arc<Lane>) -> Self {
        Self { inner, lane }
    }
}

impl Holder for QueuedAuthor {
    fn address(&self) -> &Address {
        self.inner.address()
    }

    fn asset(&self) -> &AssetId {
        self.inner.asset()
    }
}

impl Signer for QueuedAuthor {
    fn sign<'a>(&'a self, request: SignRequest) -> SignFuture<'a> {
        self.inner.sign(request)
    }
}

impl SingleSender for QueuedAuthor {
    fn send<'a>(
        &'a self,
        destination: Address,
        amount: Amount,
    ) -> FutureResult<'a, base::TransactionId> {
        Box::pin(self.lane.run(
            move || self.inner.send(destination.clone(), amount.clone()),
            |error: &WalletError| {
                error.ambiguous_transaction_id.is_none() && error.kind.retryable()
            },
            |refusal| refusal,
        ))
    }
}

impl TransactionFactory for QueuedAuthor {
    fn transaction(&self) -> Box<dyn TransactionBuilder> {
        self.inner.transaction()
    }

    fn resume(
        &self,
        snapshot: &base::TransactionSnapshot,
    ) -> Result<Box<dyn TransactionBuilder>, base::TransactionError> {
        self.inner.resume(snapshot)
    }

    fn broadcaster(&self) -> &dyn Broadcaster {
        self.inner.broadcaster()
    }
}
