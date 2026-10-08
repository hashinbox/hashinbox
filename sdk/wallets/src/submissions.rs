use tokio::task::JoinError;

use crate::{RegistrationError, RegistrationFuture, SubmissionRegistrar, SubmissionTask};

/// Wallet submission registration backed by owned Tokio tasks.
///
/// Close admission and drain submissions before stopping recovery dependencies so ambiguous
/// sends can finish recovery. Dropping this aborts remaining work; cancelling a
/// drain aborts its work. Use one shutdown driver.
#[derive(Default)]
pub struct Submissions(tasks::Tasks);

impl Submissions {
    /// An open registrar with no submissions.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Refuses new submissions while accepted work continues.
    pub fn close(&self) {
        self.0.close();
    }

    /// Closes registration and waits for all accepted submissions.
    ///
    /// # Errors
    /// Returns the first observed task failure after waiting for every task.
    pub async fn drain(&self) -> Result<(), JoinError> {
        self.0.drain().await
    }
}

impl SubmissionRegistrar for Submissions {
    fn register<'a>(&'a self, task: SubmissionTask) -> RegistrationFuture<'a> {
        Box::pin(async move {
            self.0
                .spawn(task.run())
                .map_err(|_| RegistrationError::Closed)
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
}
