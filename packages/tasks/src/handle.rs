//! A job's handle: the job by id, and an optional wait for its end.

use std::sync::Arc;

use crate::{
    storage::{Store, StoreError},
    task::{Id, Job},
};

/// One job, looked up in its store.
#[derive(Clone)]
pub struct Handle {
    id: Id,
    store: Arc<dyn Store>,
}

impl Handle {
    /// The handle of `id` in `store`.
    #[must_use]
    pub fn new(id: Id, store: Arc<dyn Store>) -> Self {
        Self { id, store }
    }

    /// The job's id.
    #[must_use]
    pub fn id(&self) -> &Id {
        &self.id
    }

    /// The job; `None` when the store does not know it.
    ///
    /// # Errors
    /// The store's failure.
    pub async fn job(&self) -> Result<Option<Job>, StoreError> {
        self.store.get(&self.id).await
    }

    /// Waits until the job finishes or `timeout` passes; the last job seen.
    ///
    /// # Errors
    /// The store's failure.
    #[cfg(feature = "runner")]
    pub async fn wait(&self, timeout: std::time::Duration) -> Result<Option<Job>, StoreError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut changes = self.store.changes();
        loop {
            let job = self.job().await?;
            if job.as_ref().is_none_or(|job| job.state.is_finished()) {
                return Ok(job);
            }
            // Any change, a lag or a closed store means: look again.
            if tokio::time::timeout_at(deadline, changes.recv())
                .await
                .is_err()
            {
                return self.job().await;
            }
        }
    }
}
