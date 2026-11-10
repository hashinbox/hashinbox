//! The executor end to end, over `Notifying<MemoryStore>`: retries with
//! saved state, timeouts, panics, per-kind concurrency, order on one topic,
//! skips, recurring jobs seeded by `before`, and stopping.

use std::{
    num::{NonZeroU32, NonZeroUsize},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use tasks::{
    Ack, Admission, Context, Executor, Id, Job, MemoryStore, Notifying, OnLock, Policy, Retry,
    Running, Status, Store, StoreError, Topic, Worker,
};

type Queue = Notifying<MemoryStore>;

/// Two attempts, retried at once.
const RETRY: Retry = Retry {
    attempts: NonZeroU32::new(2).unwrap(),
    backoff: Duration::ZERO,
};

/// A store, and an executor over it with the workers `add` adds, running.
fn start(add: impl FnOnce(Executor<Queue>) -> Executor<Queue>) -> (Queue, Running) {
    let store = Notifying::new(MemoryStore::new());
    let executor = add(Executor::new(store.clone(), Duration::from_millis(5)));
    (store, executor.run())
}

/// Enqueues a job of `W` for `input`; its id.
async fn enqueue<W: Worker>(store: &Queue, input: &W::Input) -> Id {
    match W::enqueue(store, input).await.unwrap() {
        Admission::Queued(id) => id,
        Admission::Duplicate(id) => panic!("a duplicate of {id}"),
    }
}

/// Job `id` once `done` holds, waiting at most five seconds.
async fn wait(store: &Queue, id: Id, done: impl Fn(&Job) -> bool) -> Job {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let job = store.get(id).await.unwrap().unwrap();
        if done(&job) {
            return job;
        }
        assert!(Instant::now() < deadline, "never reached: {job:?}");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

fn finished(job: &Job) -> bool {
    job.status.is_finished()
}

/// A topic of its own per input.
fn own(kind: &str, input: u32) -> Option<Topic> {
    Topic::new([kind.to_owned(), input.to_string()])
}

/// Retries its first attempt after saving how far it got; the second
/// attempt sees that state and succeeds.
struct Resumes;

impl Worker for Resumes {
    const KIND: &'static str = "resume";
    const POLICY: Policy = Policy {
        retry: RETRY,
        ..Policy::DEFAULT
    };
    type Input = u32;
    type State = u32;

    fn topic(input: &u32) -> Option<Topic> {
        own(Self::KIND, *input)
    }

    async fn run(&self, mut cx: Context<'_, u32, u32>) -> Ack {
        match cx.state() {
            Some(step) => Ack::Fail(format!("resumed at {step}")),
            None => match cx.save(7).await {
                Ok(()) => Ack::Retry("half way".into()),
                Err(error) => Ack::Fail(error.to_string()),
            },
        }
    }
}

/// Never answers within its 20 ms timeout.
struct Hangs;

impl Worker for Hangs {
    const KIND: &'static str = "hang";
    const POLICY: Policy = Policy {
        timeout: Duration::from_millis(20),
        retry: RETRY,
        ..Policy::DEFAULT
    };
    type Input = u32;
    type State = ();

    fn topic(input: &u32) -> Option<Topic> {
        own(Self::KIND, *input)
    }

    async fn run(&self, _cx: Context<'_, u32, ()>) -> Ack {
        std::future::pending().await
    }
}

/// Panics.
struct Panics;

impl Worker for Panics {
    const KIND: &'static str = "panic";
    const POLICY: Policy = Policy::DEFAULT;
    type Input = u32;
    type State = ();

    fn topic(input: &u32) -> Option<Topic> {
        own(Self::KIND, *input)
    }

    async fn run(&self, _cx: Context<'_, u32, ()>) -> Ack {
        panic!("a worker bug")
    }
}

/// Takes 20 ms, two at a time; records the most running at once and the
/// order inputs ran in.
struct Counts {
    running: AtomicUsize,
    most: AtomicUsize,
    order: Mutex<Vec<u32>>,
}

impl Counts {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            running: AtomicUsize::new(0),
            most: AtomicUsize::new(0),
            order: Mutex::new(Vec::new()),
        })
    }
}

/// [`Counts`], as a worker.
struct Counted(Arc<Counts>);

impl Worker for Counted {
    const KIND: &'static str = "count";
    const POLICY: Policy = Policy {
        concurrency: NonZeroUsize::new(2).unwrap(),
        ..Policy::DEFAULT
    };
    type Input = u32;
    type State = ();

    fn topic(input: &u32) -> Option<Topic> {
        own(Self::KIND, *input)
    }

    async fn run(&self, cx: Context<'_, u32, ()>) -> Ack {
        let counts = &self.0;
        let now = counts.running.fetch_add(1, Ordering::SeqCst) + 1;
        counts.most.fetch_max(now, Ordering::SeqCst);
        counts.order.lock().unwrap().push(*cx.input());
        tokio::time::sleep(Duration::from_millis(20)).await;
        counts.running.fetch_sub(1, Ordering::SeqCst);
        Ack::Done
    }
}

/// Holds `n.w{input}` for 200 ms.
struct Sends;

impl Worker for Sends {
    const KIND: &'static str = "send";
    const POLICY: Policy = Policy {
        concurrency: NonZeroUsize::new(4).unwrap(),
        ..Policy::DEFAULT
    };
    type Input = u32;
    type State = ();

    fn topic(input: &u32) -> Option<Topic> {
        Topic::new(["n".to_owned(), format!("w{input}")])
    }

    async fn run(&self, _cx: Context<'_, u32, ()>) -> Ack {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ack::Done
    }
}

/// Decides on the whole network `n`; gives way to a newer job on it.
struct Rounds;

impl Worker for Rounds {
    const KIND: &'static str = "round";
    const POLICY: Policy = Policy {
        on_lock: OnLock::Skip,
        unique: true,
        ..Policy::DEFAULT
    };
    type Input = ();
    type State = ();

    fn topic((): &()) -> Option<Topic> {
        Topic::parse("n")
    }

    async fn run(&self, _cx: Context<'_, (), ()>) -> Ack {
        Ack::Done
    }
}

/// Recurs every 10 ms, counting its runs; its one job is seeded when its
/// queue starts.
struct Ticks(Arc<AtomicUsize>);

impl Worker for Ticks {
    const KIND: &'static str = "tick";
    const POLICY: Policy = Policy {
        unique: true,
        ..Policy::DEFAULT
    };
    type Input = ();
    type State = ();

    fn topic((): &()) -> Option<Topic> {
        Topic::parse("tick")
    }

    async fn run(&self, _cx: Context<'_, (), ()>) -> Ack {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ack::Again(tasks::now() + 10, None)
    }

    async fn before(&self, store: &dyn Store) -> Result<(), StoreError> {
        Self::enqueue(store, &()).await.map(|_| ())
    }
}

/// Finishes only after 100 ms, flagging that it got there.
struct Slow(Arc<AtomicBool>);

impl Worker for Slow {
    const KIND: &'static str = "slow";
    const POLICY: Policy = Policy::DEFAULT;
    type Input = ();
    type State = ();

    fn topic((): &()) -> Option<Topic> {
        Topic::parse("slow")
    }

    async fn run(&self, _cx: Context<'_, (), ()>) -> Ack {
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.0.store(true, Ordering::SeqCst);
        Ack::Done
    }
}

#[tokio::test]
async fn a_retry_runs_again_from_the_state_it_saved() {
    let (store, running) = start(|executor| executor.add(Resumes));
    let id = enqueue::<Resumes>(&store, &1).await;
    let job = wait(&store, id, finished).await;
    assert_eq!(
        (job.status, job.attempts, job.reason.as_deref()),
        (Status::Failed, 2, Some("resumed at 7"))
    );
    assert_eq!(job.state, Some(serde_json::json!(7)), "the outcome kept it");
    running.stop().await;
}

#[tokio::test]
async fn a_run_past_its_timeout_is_retried_then_fails() {
    let (store, running) = start(|executor| executor.add(Hangs));
    let id = enqueue::<Hangs>(&store, &1).await;
    let job = wait(&store, id, finished).await;
    assert_eq!(
        (job.status, job.attempts, job.reason.as_deref()),
        (Status::Failed, 2, Some("attempts-exhausted: timeout"))
    );
    running.stop().await;
}

#[tokio::test]
async fn a_panic_fails_its_job_and_frees_its_topic() {
    let (store, running) = start(|executor| executor.add(Panics));
    let first = enqueue::<Panics>(&store, &1).await;
    let behind = enqueue::<Panics>(&store, &1).await;
    for id in [first, behind] {
        let job = wait(&store, id, finished).await;
        assert_eq!(
            (job.status, job.reason.as_deref()),
            (Status::Failed, Some("panicked"))
        );
    }
    running.stop().await;
}

#[tokio::test]
async fn a_kind_never_runs_more_than_its_concurrency() {
    let counts = Counts::new();
    let worker = Counted(counts.clone());
    let (store, running) = start(|executor| executor.add(worker));
    let mut ids = Vec::new();
    for input in 0..6 {
        ids.push(enqueue::<Counted>(&store, &input).await);
    }
    for id in ids {
        assert_eq!(wait(&store, id, finished).await.status, Status::Done);
    }
    assert_eq!(counts.most.load(Ordering::SeqCst), 2);
    running.stop().await;
}

#[tokio::test]
async fn jobs_on_one_topic_run_one_at_a_time_in_order() {
    let counts = Counts::new();
    let (store, running) = start(|executor| executor.add(Counted(counts.clone())));
    let mut ids = Vec::new();
    for input in 0..5 {
        let job = Job::new(Topic::parse("count.w").unwrap(), "count", input.into());
        let Admission::Queued(id) = store.enqueue(job, false).await.unwrap() else {
            panic!("queued");
        };
        ids.push(id);
    }
    for id in ids {
        wait(&store, id, finished).await;
    }
    assert_eq!(counts.most.load(Ordering::SeqCst), 1, "one at a time");
    assert_eq!(*counts.order.lock().unwrap(), [0, 1, 2, 3, 4], "in order");
    running.stop().await;
}

#[tokio::test]
async fn a_round_gives_way_to_a_newer_job_on_its_network() {
    let (store, running) = start(|executor| executor.add(Sends).add(Rounds));
    let held = enqueue::<Sends>(&store, &1).await;
    wait(&store, held, |job| job.status == Status::Running).await;
    let round = enqueue::<Rounds>(&store, &()).await;
    let again = Rounds::enqueue(&store, &()).await.unwrap();
    assert_eq!(again, Admission::Duplicate(round), "one unfinished round");
    tokio::time::sleep(Duration::from_millis(20)).await;
    let waiting = store.get(round).await.unwrap().unwrap();
    assert_eq!(waiting.status, Status::Pending, "behind the running send");

    let newer = enqueue::<Sends>(&store, &2).await;
    let skipped = wait(&store, round, finished).await;
    assert_eq!(
        (skipped.status, skipped.reason),
        (Status::Skipped, Some(format!("skipped: {newer}")))
    );
    assert_eq!(wait(&store, newer, finished).await.status, Status::Done);
    running.stop().await;
}

#[tokio::test]
async fn a_job_seeded_by_before_recurs_by_answering_again() {
    let runs = Arc::new(AtomicUsize::new(0));
    let (store, running) = start(|executor| executor.add(Ticks(runs.clone())));
    let id = Id(1);
    let deadline = Instant::now() + Duration::from_secs(5);
    while runs.load(Ordering::SeqCst) < 3 {
        assert!(Instant::now() < deadline, "ran {runs:?} times");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let job = store.get(id).await.unwrap().unwrap();
    assert!(!job.status.is_finished(), "it never ends");
    let again = Ticks::enqueue(&store, &()).await.unwrap();
    assert_eq!(again, Admission::Duplicate(id), "one, seeded once");
    running.stop().await;
}

#[tokio::test]
async fn stopping_aborts_the_runs_in_flight() {
    let reached = Arc::new(AtomicBool::new(false));
    let (store, running) = start(|executor| executor.add(Slow(reached.clone())));
    let id = enqueue::<Slow>(&store, &()).await;
    wait(&store, id, |job| job.status == Status::Running).await;
    running.stop().await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!reached.load(Ordering::SeqCst), "the run was aborted");
    let job = store.get(id).await.unwrap().unwrap();
    assert_eq!(job.status, Status::Running, "its lease brings it back");
}
