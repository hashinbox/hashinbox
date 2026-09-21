use crate::{IndexError, IndexErrorKind};

/// Chain-neutral synchronization policy shared by concrete indexers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncPolicy {
    pub minimum_confirmations: u64,
    pub reorg_retention: u64,
    pub batch_size: usize,
}

impl SyncPolicy {
    #[must_use]
    pub const fn new(minimum_confirmations: u64, reorg_retention: u64, batch_size: usize) -> Self {
        Self {
            minimum_confirmations,
            reorg_retention,
            batch_size,
        }
    }

    pub fn validate(&self) -> Result<(), IndexError> {
        if self.minimum_confirmations == 0 {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "minimum confirmations must be greater than zero",
                false,
            ));
        }
        if self.reorg_retention == 0 || self.batch_size == 0 {
            return Err(IndexError::new(
                IndexErrorKind::InvalidRequest,
                "rollback retention and batch size must be greater than zero",
                false,
            ));
        }
        Ok(())
    }
}
