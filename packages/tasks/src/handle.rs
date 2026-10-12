//! A task's handle: its status by id, and an optional polling wait.

use std::sync::Arc;

use crate::{
    storage::{Store, StoreError},
    task::{Id, Status},
};

/// One enqueued task, looked up in its store.
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

    /// The task.
    #[must_use]
    pub fn id(&self) -> &Id {
        &self.id
    }

    /// Where the task is; `None` when the store does not know it.
    ///
    /// # Errors
    /// The store's failure.
    pub async fn status(&self) -> Result<Option<Status>, StoreError> {
        self.store.status(&self.id).await
    }

    /// Polls every `poll` until the task finishes or `timeout` passes; the
    /// last status seen.
    ///
    /// # Errors
    /// The store's failure.
    #[cfg(feature = "runner")]
    pub async fn wait(
        &self,
        poll: std::time::Duration,
        timeout: std::time::Duration,
    ) -> Result<Option<Status>, StoreError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let status = self.status().await?;
            if matches!(status, Some(Status::Finished(_)) | None)
                || tokio::time::Instant::now() >= deadline
            {
                return Ok(status);
            }
            tokio::time::sleep(poll).await;
        }
    }
}
