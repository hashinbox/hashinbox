//! Settling a transaction whose submission outcome is unknown.
//!
//! Recovery is lazy: it runs on the next broadcast of that transaction, or
//! before the next send from an address it spends from. The node is asked
//! whether it has the exact transaction; if not, the identical bytes are
//! replayed. A replay the node refuses conflicts with the chain, so its
//! outputs are released and the index decides what happened.

use std::sync::Arc;

use base::TransactionError;

use super::{dispatch::Dispatch, reservation::Settlement};
use crate::native::{TransactionId, rpc::Submitter};

/// Settles unknown outcomes: accept what the node has, replay the
/// rest.
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
        let id = settlement.transaction().id;
        match self.submitter.known(&id).await {
            Ok(true) => Ok(settlement.accept()),
            Ok(false) => self.dispatch.run(settlement).await,
            Err(error) => Err(error.ambiguous(&id)),
        }
    }
}
