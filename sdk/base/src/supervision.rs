//! Supervision of send work that must outlive the request that started it: a
//! [`SubmissionTask`], its [`Activation`], and the [`SubmissionRegistrar`] the
//! application implements to own them.
//!
//! A chain whose send pipeline finishes in background work (Solana confirms
//! and recovers after the request returns) hands that work to a registrar, so
//! the application can refuse new work and drain running work on shutdown
//! without naming the chain.
//!
//! "Submission" in these names is the *work* of sending. It is a different
//! thing from [`crate::transaction::Submission`], which is one node's
//! acceptance receipt for one transaction — two concepts still sharing one
//! word, which only renaming the receipt fixes.

use std::{future::Future, pin::Pin};

use futures_channel::oneshot;

/// What [`SubmissionRegistrar::register`] returns.
pub type RegistrationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), RegistrationError>> + Send + 'a>>;

/// Submission work that must run to completion even if its caller goes away:
/// dispatching signed transactions and, after an unknown outcome, recovering
/// it. It stays dormant until its [`Activation`] starts it.
pub struct SubmissionTask(Pin<Box<dyn Future<Output = ()> + Send + 'static>>);

/// Starts a dormant [`SubmissionTask`]; dropping it cancels the task instead.
pub struct Activation(oneshot::Sender<()>);

impl SubmissionTask {
    /// A task that runs `future` only after [`Activation::start`]; the chain
    /// starts it once its registrar acknowledged the task.
    pub fn dormant(future: impl Future<Output = ()> + Send + 'static) -> (Self, Activation) {
        let (start, waiting) = oneshot::channel();
        (
            Self(Box::pin(async move {
                if waiting.await.is_ok() {
                    future.await;
                }
            })),
            Activation(start),
        )
    }

    /// Runs the task; it returns at once if it was never activated.
    pub async fn run(self) {
        self.0.await;
    }
}

impl Activation {
    /// Lets the task run.
    pub fn start(self) {
        let _ = self.0.send(());
    }
}

/// Inserts one submission task into supervision.
///
/// `Ok(())` acknowledges that `task` is already owned by the supervisor. An
/// implementation must return an error without insertion when registration
/// is closed or that acknowledgement cannot be delivered.
pub trait SubmissionRegistrar: Send + Sync {
    /// Takes ownership of `task`, or refuses it.
    fn register<'a>(&'a self, task: SubmissionTask) -> RegistrationFuture<'a>;
}

/// Why a registrar refused a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistrationError {
    /// Registration is closed, for example during shutdown.
    Closed,
    /// The supervisor may have taken the task but could not confirm it.
    AcknowledgementLost,
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use futures_executor::block_on;

    use super::*;

    #[test]
    fn a_task_runs_only_after_activation() {
        let completed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&completed);
        let (task, activation) = SubmissionTask::dormant(async move {
            flag.store(true, Ordering::Release);
        });
        activation.start();
        block_on(task.run());
        assert!(completed.load(Ordering::Acquire));

        let (task, activation) = SubmissionTask::dormant(async { unreachable!("cancelled") });
        drop(activation);
        block_on(task.run());
    }
}
