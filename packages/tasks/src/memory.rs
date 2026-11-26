//! [`MemoryStore`]: a [`Store`] in process memory: for tests, and the
//! reference of what each [`Store`] rule means.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use crate::{
    job::{Id, Job, Status},
    storage::{Admission, Claim, Store, StoreError, StoreFuture},
    worker::OnLock,
};

/// Finished jobs kept for [`Store::get`]; older ones are forgotten.
const KEPT: usize = 1_024;

/// Jobs kept in memory, gone with the process: every unfinished job, and
/// the last `KEPT` (1 024) finished ones. Clones share the jobs.
#[derive(Clone, Default)]
pub struct MemoryStore(Arc<Mutex<Line>>);

/// The jobs in line order, the finished ones oldest first, and the last id
/// given.
#[derive(Default)]
struct Line {
    jobs: BTreeMap<Id, Job>,
    finished: VecDeque<Id>,
    last: u64,
}

impl MemoryStore {
    /// No jobs.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The jobs, whatever a panicking holder left: every write is one
    /// assignment.
    fn line(&self) -> MutexGuard<'_, Line> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Line {
    /// Keeps `job`, forgetting the oldest finished job past [`KEPT`].
    fn keep(&mut self, job: Job) {
        if job.status.is_finished() {
            self.finished.push_back(job.id);
        }
        self.jobs.insert(job.id, job);
        while self.finished.len() > KEPT {
            let Some(oldest) = self.finished.pop_front() else {
                break;
            };
            if self
                .jobs
                .get(&oldest)
                .is_some_and(|job| job.status.is_finished())
            {
                self.jobs.remove(&oldest);
            }
        }
    }

    /// The first conflicting job blocking `job` at `now`: one running, or
    /// one ahead of it holding its place.
    fn blocker(&self, job: &Job, now: u64) -> Option<Id> {
        self.jobs
            .values()
            .find(|other| other.blocks(job, now))
            .map(|other| other.id)
    }

    /// The unfinished job of `job`'s kind on its topic, if any.
    fn holder(&self, job: &Job) -> Option<Id> {
        self.jobs
            .values()
            .find(|kept| {
                !kept.status.is_finished() && kept.kind == job.kind && kept.topic == job.topic
            })
            .map(|kept| kept.id)
    }
}

/// `value` as a resolved store future.
fn ready<'a, T: Send + 'a>(value: Result<T, StoreError>) -> StoreFuture<'a, T> {
    Box::pin(std::future::ready(value))
}

impl Store for MemoryStore {
    fn enqueue(&self, mut job: Job, unique: bool) -> StoreFuture<'_, Admission> {
        let mut line = self.line();
        if unique && let Some(held) = line.holder(&job) {
            return ready(Ok(Admission::Duplicate(held)));
        }
        line.last += 1;
        job.id = Id(line.last);
        let id = job.id;
        line.keep(job);
        ready(Ok(Admission::Queued(id)))
    }

    fn claim(&self, claim: Claim<'_>) -> StoreFuture<'_, Vec<Job>> {
        let Claim {
            kind,
            limit,
            on_lock,
            now,
            until,
        } = claim;
        let mut line = self.line();
        let past: Vec<Job> = line
            .jobs
            .values()
            .filter(|job| job.kind == kind && job.is_expired(now))
            .cloned()
            .collect();
        for job in past {
            line.keep(job.fail("expired".into()));
        }
        let due: Vec<Id> = line
            .jobs
            .values()
            .filter(|job| job.kind == kind && job.is_due(now))
            .map(|job| job.id)
            .collect();
        let mut claimed = Vec::new();
        for id in due {
            if claimed.len() == limit {
                break;
            }
            let job = line.jobs[&id].clone();
            if let Some(holder) = line.blocker(&job, now) {
                if on_lock == OnLock::Skip {
                    line.keep(job.skip(holder));
                }
                continue;
            }
            let running = Job {
                status: Status::Running,
                run_at: until,
                attempts: job.attempts.saturating_add(1),
                ..job
            };
            line.keep(running.clone());
            claimed.push(running);
        }
        ready(Ok(claimed))
    }

    fn update(&self, job: Job, attempt: u32) -> StoreFuture<'_, ()> {
        let mut line = self.line();
        let running = line
            .jobs
            .get(&job.id)
            .is_some_and(|kept| kept.status == Status::Running && kept.attempts == attempt);
        if !running {
            return ready(Err(StoreError::Stale));
        }
        line.keep(job);
        ready(Ok(()))
    }

    fn get(&self, id: Id) -> StoreFuture<'_, Option<Job>> {
        ready(Ok(self.line().jobs.get(&id).cloned()))
    }
}
