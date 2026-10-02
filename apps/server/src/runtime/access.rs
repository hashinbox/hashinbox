//! Admissions serialize maintenance against network operations, not against other networks.
use crate::config::AnyError;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(crate) struct Access {
    blocked: AtomicBool,
    permits: Arc<Semaphore>,
}
impl Access {
    const CAPACITY: u32 = 1024;
    pub(crate) fn new() -> Self {
        Self {
            blocked: AtomicBool::new(false),
            permits: Arc::new(Semaphore::new(Self::CAPACITY as usize)),
        }
    }
    pub(crate) fn enter(&self) -> Result<OwnedSemaphorePermit, AnyError> {
        if self.blocked.load(Ordering::Acquire) {
            return Err("network maintenance in progress".into());
        }
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| "network is busy or undergoing maintenance")?;
        if self.blocked.load(Ordering::Acquire) {
            return Err("network maintenance in progress".into());
        }
        Ok(permit)
    }
    pub(crate) fn block(&self) {
        self.blocked.store(true, Ordering::Release);
    }
    pub(crate) fn open(&self) {
        self.blocked.store(false, Ordering::Release);
    }
    pub(crate) async fn idle(&self) -> Result<OwnedSemaphorePermit, AnyError> {
        self.block();
        Ok(self
            .permits
            .clone()
            .acquire_many_owned(Self::CAPACITY)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn maintenance_drains_one_network_without_blocking_another() {
        let network = Access::new();
        let other = Access::new();
        let operation = network.enter().unwrap();
        let maintenance = network.idle();
        tokio::pin!(maintenance);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(maintenance.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert!(network.enter().is_err());
        assert!(other.enter().is_ok());
        drop(operation);
        let drained = maintenance.await.unwrap();
        assert!(network.enter().is_err());
        drop(drained);
        assert!(network.enter().is_err());
        network.open();
        assert!(network.enter().is_ok());
    }
}
