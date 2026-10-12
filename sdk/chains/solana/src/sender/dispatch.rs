//! The submit stage as it runs inside a registered [`super::SubmissionTask`]:
//! the exact signed bytes, in order, each rebroadcast to every configured
//! node until it lands or its blockhash expires.

use std::{sync::Arc, time::Duration};

use wallets::{SendError, WalletError, WalletErrorKind};

use super::reservation::PreparedTransaction;
use crate::native::rpc::Submitter;

/// Pause between rebroadcasts of the same bytes.
const REBROADCAST: Duration = Duration::from_secs(1);

/// The dispatch half of submit: exact bytes, in order, while they are live.
/// Every read and send is at the transaction's simulated floor, never at the
/// older slot it was built at.
pub(super) struct Dispatch {
    pub(super) submitter: Arc<dyn Submitter>,
}

impl Dispatch {
    /// Sends each transaction in order and stops at the first that was not
    /// seen landed. That one fails as [`WalletErrorKind::OutcomeUnknown`]: it
    /// may still have landed, so the error carries its ID.
    pub(super) async fn run(
        &self,
        transactions: Vec<PreparedTransaction>,
    ) -> Result<Vec<base::TransactionId>, SendError> {
        let mut accepted = Vec::with_capacity(transactions.len());
        for (index, prepared) in transactions.into_iter().enumerate() {
            if index > 0 {
                self.live(index, &prepared, accepted.clone(), "dispatch")
                    .await?;
            }
            if !self.land(&prepared).await {
                let mut source = WalletError::new(
                    WalletErrorKind::OutcomeUnknown,
                    "Solana transaction was not seen landed before its blockhash expired; \
                     check history",
                );
                source.ambiguous_transaction_id = Some(prepared.transaction.id.into());
                return Err(SendError::item(index, accepted, source));
            }
            accepted.push(prepared.transaction.id.into());
        }
        Ok(accepted)
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

    /// Rebroadcasts the same bytes until a node has seen them land (`true`)
    /// or the blockhash expired or the height is unreadable (`false`).
    async fn land(&self, prepared: &PreparedTransaction) -> bool {
        let PreparedTransaction {
            transaction,
            floor_slot,
        } = prepared;
        loop {
            let _ = self.submitter.broadcast(transaction, *floor_slot).await;
            if let Ok(true) = self.submitter.known(&transaction.id, *floor_slot).await {
                return true;
            }
            match self.submitter.height(*floor_slot).await {
                Ok(height) if !transaction.lifetime.expired(height) => {}
                _ => return false,
            }
            tokio::time::sleep(REBROADCAST).await;
        }
    }
}
