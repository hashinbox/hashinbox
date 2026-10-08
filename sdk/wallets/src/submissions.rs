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
