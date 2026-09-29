//! The submit stage as it runs inside a registered [`super::SubmissionTask`]:
//! the exact signed bytes, in order, while their blockhash is live, each
//! sent up to three times.

use std::sync::Arc;

use wallets::{SendError, WalletError, WalletErrorKind};

use super::reservation::Settlement;
use crate::native::{SignedTransaction, rpc::Submitter};

/// The dispatch half of submit: exact bytes, in order, while they are live.
pub(super) struct Dispatch {
    pub(super) submitter: Arc<dyn Submitter>,
    pub(super) min_slot: u64,
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
        transactions: Vec<SignedTransaction>,
        settlement: Settlement,
    ) -> Outcome {
        let mut accepted = Vec::with_capacity(transactions.len());
        for (index, transaction) in transactions.into_iter().enumerate() {
            if index > 0
                && let Err(error) = self
                    .live(index, &transaction, accepted.clone(), "dispatch")
                    .await
            {
                return Outcome::Complete(Err(error));
            }
            if !self.submit(&transaction).await {
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
            accepted.push(transaction.id.into());
        }
        Outcome::Complete(Ok(accepted))
    }

    /// Fails definitely unless the chain is still below `transaction`'s
    /// last valid height; `stage` names when the check ran.
    pub(super) async fn live(
        &self,
        index: usize,
        transaction: &SignedTransaction,
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
        let height =
            self.submitter.height(self.min_slot).await.map_err(|_| {
                definite(format!("Solana block height is unavailable before {stage}"))
            })?;
        if transaction.lifetime.expired(height) {
            return Err(definite(format!(
                "Solana transaction lifetime expired before {stage}"
            )));
        }
        Ok(())
    }

    /// Up to three identical attempts; `true` once the node accepted or has
    /// seen the transaction. Replays stop once the status read fails or the
    /// lifetime ends.
    async fn submit(&self, transaction: &SignedTransaction) -> bool {
        for attempt in 0..3 {
            if self
                .submitter
                .broadcast(transaction, self.min_slot)
                .await
                .is_ok()
            {
                return true;
            }
            match self.submitter.known(&transaction.id, self.min_slot).await {
                Ok(true) => return true,
                Ok(false) if attempt < 2 => {}
                _ => return false,
            }
            match self.submitter.height(self.min_slot).await {
                Ok(height) if !transaction.lifetime.expired(height) => {}
                _ => return false,
            }
        }
        false
    }
}
