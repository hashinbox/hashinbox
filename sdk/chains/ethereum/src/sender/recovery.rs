//! Settling an envelope whose submission outcome is unknown.
//!
//! Recovery is lazy: it runs on the next broadcast of that envelope, or
//! before the next send from its sender, which waits for it. The node is
//! asked whether it knows the exact hash; if not, the identical bytes are
//! replayed, so the retained nonce is never reused by another transaction.

use std::sync::Arc;

use base::TransactionError;

use super::{dispatch::Dispatch, reservation::Settlement};
use crate::native::{Error, TransactionId, rpc::Submitter};

/// Settles unknown outcomes: accept what the node knows, replay
/// the rest.
pub(super) struct Recovery {
    pub(super) submitter: Arc<dyn Submitter>,
    pub(super) dispatch: Dispatch,
}

impl Recovery {
    /// Settles `settlement`, whose earlier submission outcome is unknown.
    pub(super) async fn settle(
        &self,
        settlement: Settlement,
    ) -> Result<TransactionId, TransactionError> {
        let id = settlement.id().clone();
        match self.submitter.known(&id).await {
            Ok(true) => settlement.accept().map_err(Error::definite),
            Ok(false) => self.dispatch.run(settlement).await,
            Err(error) => Err(error.ambiguous(&id)),
        }
    }
}
