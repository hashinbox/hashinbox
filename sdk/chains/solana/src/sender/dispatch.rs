//! The submit stage as it runs inside a registered [`super::SubmissionTask`]:
//! the exact signed bytes, in order, while their blockhash is live, each
//! sent up to three times.

use std::sync::Arc;

use wallets::{SendError, WalletError, WalletErrorKind};

use super::reservation::{PreparedTransaction, Settlement};
use crate::native::{SignedTransaction, rpc::Submitter};

/// The dispatch half of submit: exact bytes, in order, while they are live.
/// Every read and send is at the transaction's simulated floor, never at the
/// older slot it was built at.
pub(super) struct Dispatch {
    pub(super) submitter: Arc<dyn Submitter>,
}

pub(super) enum Outcome {
    Complete(Result<Vec<base::TransactionId>, SendError>),
    Unknown {
        error: SendError,
        transaction: Box<SignedTransaction>,
    },
}

impl Dispatch {
    /// Sends each transaction in order and stops at the first that did not
    /// land. An unknown outcome keeps its sender reserved; every other
    /// sender is released when `settlement` drops.
    pub(super) async fn run(
        &self,
        transactions: Vec<PreparedTransaction>,
        settlement: Settlement,
    ) -> Outcome {
        let mut accepted = Vec::with_capacity(transactions.len());
        for (index, prepared) in transactions.into_iter().enumerate() {
            if index > 0
                && let Err(error) = self
                    .live(index, &prepared, accepted.clone(), "dispatch")
                    .await
            {
                return Outcome::Complete(Err(error));
            }
            if !self.submit(&prepared).await {
                let transaction = prepared.transaction;
                settlement.retain(&transaction.from);
                let mut source = WalletError::new(
                    WalletErrorKind::Unavailable,
                    "Solana submission outcome is unknown",
                );
                source.ambiguous_transaction_id = Some(transaction.id.into());
                return Outcome::Unknown {
                    error: SendError::item(index, accepted, source),
                    transaction: Box::new(transaction),
                };
            }
            accepted.push(prepared.transaction.id.into());
        }
        Outcome::Complete(Ok(accepted))
    }

    /// Fails definitely unless the chain is still below the transaction's
    /// last valid height; `stage` names when the check ran.
    pub(super) async fn live(
        &self,
        index: usize,
        prepared: &PreparedTransaction,
        accepted: Vec<base::TransactionId>,
        stage: &str,
    ) -> Result<(), SendError> {
        let definite = |message: String| {
            SendError::item(
                index,
                accepted.clone(),
                WalletError::new(WalletErrorKind::Unavailable, message),
            )
        };
        let height = self
            .submitter
            .height(prepared.floor_slot)
            .await
            .map_err(|_| definite(format!("Solana block height is unavailable before {stage}")))?;
        if prepared.transaction.lifetime.expired(height) {
            return Err(definite(format!(
                "Solana transaction lifetime expired before {stage}"
            )));
        }
        Ok(())
    }

    /// Up to three identical attempts; `true` once the node accepted or has
    /// seen the transaction. Replays stop once the status read fails or the
    /// lifetime ends.
    async fn submit(&self, prepared: &PreparedTransaction) -> bool {
        let PreparedTransaction {
            transaction,
            floor_slot,
        } = prepared;
        for attempt in 0..3 {
            if self
                .submitter
                .broadcast(transaction, *floor_slot)
                .await
                .is_ok()
            {
                return true;
            }
            match self.submitter.known(&transaction.id, *floor_slot).await {
                Ok(true) => return true,
                Ok(false) if attempt < 2 => {}
                _ => return false,
            }
            match self.submitter.height(*floor_slot).await {
                Ok(height) if !transaction.lifetime.expired(height) => {}
                _ => return false,
            }
        }
        false
    }
}
