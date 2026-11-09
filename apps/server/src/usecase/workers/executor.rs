//! The queue's executor: which worker runs a job, how many run at once, and
//! the loop that starts due jobs and stores how they ended.

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Duration};

use tasks::{Ack, Changes as _, Job, Retry, Status, Store, Task, Ticking, Worker};
use tokio::task::JoinSet;

use crate::usecase::tasks::Queue;

/// Jobs run at once, of topics that do not conflict.
const CONCURRENCY: usize = 4;

/// The wait after a store failure.
const PAUSE: Duration = Duration::from_secs(1);

/// How often due retries are looked for without a write.
pub(crate) const TICK: Duration = Duration::from_secs(1);

/// A worker behind its types: one run of a job, answered.
type Run =
    Arc<dyn Fn(Job, Arc<dyn Store>) -> Pin<Box<dyn Future<Output = Ack> + Send>> + Send + Sync>;

/// Runs the queue's jobs with a worker and retry rule per kind.
pub(crate) struct Executor {
    queue: Queue,
    tick: Duration,
    kinds: HashMap<&'static str, (Run, Retry)>,
}

impl Executor {
    /// No workers, over `queue`, looking for due jobs every `tick`
    /// ([`TICK`]) without a write.
    pub(crate) fn new(queue: Queue, tick: Duration) -> Self {
        Self {
            queue,
            tick,
            kinds: HashMap::new(),
        }
    }

    /// Runs jobs of `W::KIND` with `worker`, retried by `retry`.
    pub(crate) fn add<W: Worker>(mut self, worker: W, retry: Retry) -> Self {
        let worker = Arc::new(worker);
        let run: Run = Arc::new(move |job, store| {
            let worker = worker.clone();
            Box::pin(async move {
                match Task::new(job, store) {
                    Ok(task) => worker.run(task).await,
                    Err(ack) => ack,
                }
            })
        });
        self.kinds.insert(W::KIND, (run, retry));
        self
    }

    /// Returns jobs a dead process left running to pending, then starts due
    /// jobs until dropped. A store failure pauses it, never stops it.
    ///
    /// # Errors
    ///
    /// When the recovery fails.
    pub(crate) async fn run(self) -> anyhow::Result<()> {
        self.queue.recover().await?;
        let ticking = Ticking::new(&self.queue, self.tick);
        // Subscribe first: a change between a look and the wait is not lost.
        let mut signal = ticking.changes();
        let mut running = JoinSet::new();
        loop {
            if self.start(&mut running).await.is_err() {
                tokio::time::sleep(PAUSE).await;
            }
            tokio::select! {
                _ = signal.recv() => {}
                Some(_) = running.join_next(), if !running.is_empty() => {}
            }
        }
    }

    /// Starts due jobs, oldest first, while there is room.
    async fn start(&self, running: &mut JoinSet<()>) -> Result<(), tasks::StoreError> {
        let due = self.queue.next(tasks::now()).await?;
        for job in due
            .into_iter()
            .take(CONCURRENCY.saturating_sub(running.len()))
        {
            let job = job.start(tasks::now());
            self.queue.update(job.clone()).await?;
            if job.status == Status::Running {
                running.spawn(self.execute(job));
            }
        }
        Ok(())
    }

    /// Runs `job` to its end and stores it settled. An unknown kind fails
    /// `unknown-kind`, a panic `panicked`; storing is retried until it lands,
    /// so a topic is never left locked.
    fn execute(&self, job: Job) -> impl Future<Output = ()> + Send + 'static {
        let queue = self.queue.clone();
        let kind = self.kinds.get(job.kind.as_str()).cloned();
        async move {
            let id = job.id.clone();
            let (ack, retry) = match kind {
                None => (Ack::Fail("unknown-kind".into()), Retry::default()),
                // Its own task, so a panic is a failure and not a locked topic.
                Some((run, retry)) => {
                    let ran = tokio::spawn(run(job, Arc::new(queue.clone()))).await;
                    (ran.unwrap_or_else(|_| Ack::Fail("panicked".into())), retry)
                }
            };
            loop {
                // Reread: the worker may have saved its state.
                let stored = match queue.get(&id).await {
                    Ok(Some(job)) => {
                        queue
                            .update(job.settle(ack.clone(), &retry, tasks::now()))
                            .await
                    }
                    Ok(None) => return,
                    Err(error) => Err(error),
                };
                if stored.is_ok() {
                    return;
                }
                tokio::time::sleep(PAUSE).await;
            }
        }
    }
}
