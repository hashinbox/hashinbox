//! The in-process driver: the same semantics as SQLite under one mutex.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Mutex, MutexGuard, PoisonError},
};

use super::{Store, StoreError, StoreFuture, retry_at, validate};
use crate::task::{Claimed, Id, NewTask, Outcome, Settlement, Status, TaskResult};

/// Tasks and results in memory; lost with the process.
#[derive(Default)]
pub struct Memory(Mutex<State>);

#[derive(Default)]
struct State {
    /// Live tasks in id (creation) order.
    queue: BTreeMap<Id, Row>,
    results: HashMap<Id, TaskResult>,
}

struct Row {
    task: NewTask,
    attempts: u32,
    lease: Option<(String, u64)>,
    error: Option<String>,
}

impl State {
    fn finish(
        &mut self,
        id: &Id,
        outcome: Outcome,
        output: Option<Vec<u8>>,
        error: Option<String>,
        now: u64,
    ) {
        if let Some(row) = self.queue.remove(id) {
            let result = TaskResult {
                task: id.clone(),
                outcome,
                output,
                error: error.or(row.error),
                attempts: row.attempts,
                finished_at: now,
            };
            self.results.insert(id.clone(), result);
        }
    }

    /// Each lane's oldest live task.
    fn heads(&self) -> Vec<Id> {
        let mut seen = std::collections::HashSet::new();
        self.queue
            .iter()
            .filter(|(_, row)| seen.insert(row.task.lane.clone()))
            .map(|(id, _)| id.clone())
            .collect()
    }
}

impl Memory {
    /// An empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // Every critical section is a complete edit; nothing panics half-way.
    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Store for Memory {
    fn enqueue<'a>(&'a self, task: NewTask, _now: u64) -> StoreFuture<'a, Id> {
        Box::pin(async move {
            validate(&task)?;
            let id = Id::new();
            let row = Row {
                task,
                attempts: 0,
                lease: None,
                error: None,
            };
            self.state().queue.insert(id.clone(), row);
            Ok(id)
        })
    }

    fn claim<'a>(
        &'a self,
        owner: &'a str,
        lease_until: u64,
        now: u64,
    ) -> StoreFuture<'a, Option<Claimed>> {
        Box::pin(async move {
            let mut state = self.state();
            for id in state.heads() {
                let row = &state.queue[&id];
                let lapsed = row.lease.as_ref().is_none_or(|(_, until)| *until < now);
                if !lapsed {
                    continue;
                }
                if row.task.deadline.is_some_and(|deadline| deadline <= now) {
                    state.finish(&id, Outcome::Expired, None, None, now);
                } else if row.lease.is_some() && row.attempts >= row.task.policy.max_attempts {
                    state.finish(
                        &id,
                        Outcome::Failed,
                        None,
                        Some("lease expired".into()),
                        now,
                    );
                }
            }
            let due = state.heads().into_iter().find(|id| {
                let row = &state.queue[id];
                row.task.run_at <= now && row.lease.as_ref().is_none_or(|(_, until)| *until < now)
            });
            let Some(id) = due else { return Ok(None) };
            let Some(row) = state.queue.get_mut(&id) else {
                return Ok(None);
            };
            row.attempts = row.attempts.saturating_add(1);
            row.lease = Some((owner.to_owned(), lease_until));
            Ok(Some(Claimed {
                id,
                lane: row.task.lane.clone(),
                kind: row.task.kind.clone(),
                payload: row.task.payload.clone(),
                attempt: row.attempts,
            }))
        })
    }

    fn settle<'a>(
        &'a self,
        id: &'a Id,
        owner: &'a str,
        settlement: Settlement,
        now: u64,
    ) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let mut state = self.state();
            let Some(row) = state.queue.get_mut(id) else {
                return Err(StoreError::Lost);
            };
            if row.lease.as_ref().is_none_or(|(holder, _)| holder != owner) {
                return Err(StoreError::Lost);
            }
            match settlement {
                Settlement::Finish {
                    outcome,
                    output,
                    error,
                } => state.finish(id, outcome, output, error, now),
                Settlement::Retry { error } if row.attempts >= row.task.policy.max_attempts => {
                    state.finish(id, Outcome::Failed, None, Some(error), now);
                }
                Settlement::Retry { error } => {
                    let backoff =
                        u64::try_from(row.task.policy.backoff.as_millis()).unwrap_or(u64::MAX);
                    row.task.run_at = retry_at(now, backoff, row.attempts);
                    row.lease = None;
                    row.error = Some(error);
                }
            }
            Ok(())
        })
    }

    fn cancel<'a>(&'a self, id: &'a Id, now: u64) -> StoreFuture<'a, bool> {
        Box::pin(async move {
            let mut state = self.state();
            if state.queue.get(id).is_none_or(|row| row.lease.is_some()) {
                return Ok(false);
            }
            state.finish(id, Outcome::Cancelled, None, None, now);
            Ok(true)
        })
    }

    fn status<'a>(&'a self, id: &'a Id) -> StoreFuture<'a, Option<Status>> {
        Box::pin(async move {
            let state = self.state();
            if let Some(row) = state.queue.get(id) {
                return Ok(Some(if row.lease.is_some() {
                    Status::Running
                } else {
                    Status::Pending
                }));
            }
            Ok(state.results.get(id).cloned().map(Status::Finished))
        })
    }
}
