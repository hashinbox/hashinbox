//! The submit stage for one settlement: broadcast its exact bytes and settle
//! it by the outcome.

use std::sync::Arc;

use base::TransactionError;

use super::reservation::Settlement;
use crate::native::{Error, TransactionId, rpc::Submitter};

/// Broadcasts a settlement's envelope, once each.
#[derive(Clone)]
pub(super) struct Dispatch {
    pub(super) submitter: Arc<dyn Submitter>,
}

impl Dispatch {
    /// Sends `settlement`'s exact bytes. Acceptance raises the sender's nonce
    /// floor. A definite refusal of a first attempt frees the nonce; every
    /// other failure, and any failure of a replay, keeps the envelope as an
    /// unknown outcome carrying its local ID.
    pub(super) async fn run(
        &self,
        settlement: Settlement,
    ) -> Result<TransactionId, TransactionError> {
        let id = settlement.id().clone();
        match self
            .submitter
            .broadcast(settlement.transaction().clone())
            .await
        {
            Ok(returned) if returned == id => settlement.accept().map_err(Error::definite),
            Ok(_) => Err(Error::rpc(
                "Ethereum node returned a different hash for the exact signed envelope",
                true,
            )
            .ambiguous(&id)),
            Err(error) if settlement.unknown() => Err(error.ambiguous(&id)),
            Err(error) => {
                let error = error.submission(&id);
                if error.ambiguous_transaction_id.is_none() {
                    settlement.reject();
                }
                Err(error)
            }
        }
    }
}
