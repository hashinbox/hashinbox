use std::sync::{Mutex, MutexGuard, PoisonError};

use tokio::task::{JoinError, JoinSet};

use crate::{RegistrationError, RegistrationFuture, SubmissionRegistrar, SubmissionTask};

/// The registrar an application uses unless it needs its own: it owns every
/// background send task a chain hands over, refuses new ones once
/// [`Self::close`] is called, and waits for the running ones in
/// [`Self::drain`] (cargo feature `submissions`).
///
/// A task is spawned on the Tokio runtime that polls its registration, and it
/// is owned before the registration is acknowledged, so an acknowledged task
/// cannot be lost and a refused one was never started.
///
/// Dropping this aborts whatever is still running. Call [`Self::drain`] before
/// the process exits: a send whose chain finishes it in the background must
/// not be cut off halfway.
#[derive(Default)]
pub struct Submissions(Mutex<Tasks>);

/// Everything registered and not yet joined.
struct Tasks {
    running: JoinSet<()>,
    /// Whether a registration is still accepted.
    open: bool,
    /// The first registered task that panicked, kept for [`Submissions::drain`].
    failure: Option<JoinError>,
}

impl Default for Tasks {
    fn default() -> Self {
        Self {
            running: JoinSet::new(),
            open: true,
            failure: None,
        }
    }
}

impl Tasks {
    /// Joins every task that has already ended, so a long-lived process holds
    /// the running ones and not every one it ever ran.
    fn reap(&mut self) {
        while let Some(ended) = self.running.try_join_next() {
            if let Err(error) = ended {
                self.failure.get_or_insert(error);
            }
        }
    }
}

impl Submissions {
    /// A registrar that accepts work and owns none yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Refuses every later registration. What is already registered keeps
    /// running until [`Self::drain`] has waited for it.
    ///
    /// A shutdown sequence calls this before it drains its request handlers,
    /// so no handler still in flight hands over work nothing will wait for.
    pub fn close(&self) {
        self.tasks().open = false;
    }

    /// Closes registration and waits for every registered task.
    ///
    /// Every task is waited for even when one of them panicked, because the
    /// others are sends a node may already have accepted.
    ///
    /// # Errors
    ///
    /// The first registered task that panicked, whenever it did.
    pub async fn drain(&self) -> Result<(), JoinError> {
        let (mut running, mut failure) = {
            let mut tasks = self.tasks();
            tasks.open = false;
            (std::mem::take(&mut tasks.running), tasks.failure.take())
        };
        while let Some(ended) = running.join_next().await {
            if let Err(error) = ended {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// The tasks, whatever a panicking holder left behind: nothing runs under
    /// this lock that could leave them half-written.
    fn tasks(&self) -> MutexGuard<'_, Tasks> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl SubmissionRegistrar for Submissions {
    fn register<'a>(&'a self, task: SubmissionTask) -> RegistrationFuture<'a> {
        Box::pin(async move {
            let mut tasks = self.tasks();
            if !tasks.open {
                return Err(RegistrationError::Closed);
            }
            tasks.reap();
            tasks.running.spawn(task.run());
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    /// A started task that counts itself in `completed` when it ends.
    fn counting(completed: &Arc<AtomicUsize>) -> SubmissionTask {
        let completed = Arc::clone(completed);
        let (task, activation) = SubmissionTask::dormant(async move {
            completed.fetch_add(1, Ordering::Release);
        });
        activation.start();
        task
    }

    /// A started task that panics.
    fn panicking() -> SubmissionTask {
        let (task, activation) = SubmissionTask::dormant(async {
            panic!("a submission task that fails");
        });
        activation.start();
        task
    }

    #[tokio::test]
    async fn a_registered_task_is_owned_until_it_is_drained() {
        let submissions = Submissions::new();
        let completed = Arc::new(AtomicUsize::new(0));

        submissions
            .register(counting(&completed))
            .await
            .expect("an open registrar takes the task");
        submissions.drain().await.expect("no task panicked");

        assert_eq!(completed.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn closing_refuses_new_work_and_keeps_what_is_running() {
        let submissions = Submissions::new();
        let completed = Arc::new(AtomicUsize::new(0));
        submissions
            .register(counting(&completed))
            .await
            .expect("registered before the close");

        submissions.close();

        assert_eq!(
            submissions.register(counting(&completed)).await,
            Err(RegistrationError::Closed)
        );
        submissions.drain().await.expect("no task panicked");
        assert_eq!(
            completed.load(Ordering::Acquire),
            1,
            "the refused task never ran, and the accepted one ran to its end"
        );
    }

    #[tokio::test]
    async fn draining_closes_registration() {
        let submissions = Submissions::new();
        submissions.drain().await.expect("nothing to wait for");

        let refused = submissions
            .register(counting(&Arc::new(AtomicUsize::new(0))))
            .await;

        assert_eq!(refused, Err(RegistrationError::Closed));
    }

    #[tokio::test]
    async fn one_panicked_task_is_reported_and_costs_no_other_task_its_end() {
        let submissions = Submissions::new();
        let completed = Arc::new(AtomicUsize::new(0));
        submissions.register(panicking()).await.expect("registered");
        // Registered after the panicking one has ended, so reaping is what
        // finds the failure and draining is what still has to report it.
        tokio::task::yield_now().await;
        submissions
            .register(counting(&completed))
            .await
            .expect("a panic elsewhere closes nothing");

        let failure = submissions
            .drain()
            .await
            .expect_err("a panicked task is reported");

        assert!(failure.is_panic());
        assert_eq!(completed.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn ended_tasks_are_released_as_new_ones_arrive() {
        let submissions = Submissions::new();
        let completed = Arc::new(AtomicUsize::new(0));
        for ended in 1..=8 {
            submissions
                .register(counting(&completed))
                .await
                .expect("registered");
            // Let this task end before the next registration looks for it.
            while completed.load(Ordering::Acquire) < ended {
                tokio::task::yield_now().await;
            }
        }

        assert_eq!(
            submissions.tasks().running.len(),
            1,
            "only the last registration is still held"
        );
        submissions.drain().await.expect("no task panicked");
        assert_eq!(completed.load(Ordering::Acquire), 8);
    }
}
