//! The submit stage for one settlement: broadcast its exact bytes and settle
//! it by the outcome.

use std::sync::Arc;

use base::TransactionError;

use super::reservation::Settlement;
use crate::native::{Error, TransactionId, rpc::Submitter, transaction::FeeRate};

/// Broadcasts a settlement's envelope, once each, under a fee ceiling.
#[derive(Clone)]
pub(super) struct Dispatch {
    pub(super) submitter: Arc<dyn Submitter>,
    pub(super) max_fee_rate: FeeRate,
}

impl Dispatch {
    /// Sends `settlement`'s exact bytes. Acceptance keeps the outputs reserved
    /// until their spend is indexed. A definite refusal frees them; for a
    /// replay that is a conflict, and the first attempt may still have
    /// landed, so the error stays ambiguous. Any other failure keeps the
    /// envelope as an unknown outcome carrying its local ID.
    pub(super) async fn run(
        &self,
        settlement: Settlement,
    ) -> Result<TransactionId, TransactionError> {
        let transaction = settlement.transaction().clone();
        let id = transaction.id;
        match self
            .submitter
            .broadcast(transaction, self.max_fee_rate)
            .await
        {
            Ok(_) => Ok(settlement.accept()),
            Err(error) => {
                let replay = settlement.unknown();
                let error = error.submission(&id);
                if error.ambiguous_transaction_id.is_none() {
                    settlement.reject();
                    if replay {
                        return Err(Error::rpc(error.message, false).ambiguous(&id));
                    }
                }
                Err(error)
            }
        }
    }
}
