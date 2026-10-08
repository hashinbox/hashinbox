//! Submission: the chain's only external effect, and the reads that judge it.

use base::BoxFuture;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Client, Reading};
use crate::native::{Error, SignedTransaction, TransactionId};

/// Transaction submission and the reads that settle its outcome.
pub trait Submitter: Send + Sync {
    /// Submits the exact signed bytes to every node the transport reaches,
    /// each exactly once, with node preflight and no node rebroadcast, and
    /// checks the answering node returns the local signature.
    ///
    /// Every failure is [`crate::native::ErrorKind::RpcUnavailable`]: the
    /// bytes may have reached a node, so the outcome is unknown and only
    /// `transaction.id` identifies it.
    fn broadcast<'a>(
        &'a self,
        transaction: &'a SignedTransaction,
        min_slot: u64,
    ) -> BoxFuture<'a, Result<(), Error>>;

    /// Whether the node has seen `transaction` land, successfully or not, at
    /// or after the slot `min_slot`.
    fn known<'a>(
        &'a self,
        transaction: &'a TransactionId,
        min_slot: u64,
    ) -> BoxFuture<'a, Result<bool, Error>>;

    /// The confirmed block height, read at or after the slot `min_slot`.
    fn height(&self, min_slot: u64) -> BoxFuture<'_, Result<u64, Error>>;
}

/// Where a transaction landed, as `getSignatureStatuses` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignatureStatus {
    /// The slot that processed it.
    pub slot: u64,
    /// Whether execution failed; a failed transaction still paid its fee.
    pub failed: bool,
}

/// A status as the node spells it. [`SignatureStatus`] is what is left once
/// [`StatusWire::coherent`] has checked it against the slots of the read: the
/// commitment and the confirmation count are evidence for that check only.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusWire {
    slot: u64,
    confirmations: Option<u64>,
    err: Option<Value>,
    confirmation_status: String,
}

impl<C: json_rpc::Client> Submitter for Client<C> {
    fn broadcast<'a>(
        &'a self,
        transaction: &'a SignedTransaction,
        min_slot: u64,
    ) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(async move {
            let unknown = || Error::rpc("Solana submission outcome is unknown", true);
            let returned = self
                .broadcast_once::<String>(
                    "sendTransaction",
                    json!([STANDARD.encode(transaction.envelope.as_bytes()), {
                        "encoding":"base64",
                        "skipPreflight":false,
                        "preflightCommitment":"confirmed",
                        "minContextSlot":min_slot,
                        "maxRetries":0
                    }]),
                )
                .await
                .map_err(|_| unknown())?;
            if returned.parse::<TransactionId>().ok() != Some(transaction.id) {
                return Err(unknown());
            }
            Ok(())
        })
    }

    fn known<'a>(
        &'a self,
        transaction: &'a TransactionId,
        min_slot: u64,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move {
            Ok(self
                .signature_status(transaction, min_slot)
                .await?
                .value
                .is_some())
        })
    }

    fn height(&self, min_slot: u64) -> BoxFuture<'_, Result<u64, Error>> {
        Box::pin(async move {
            self.request(
                "getBlockHeight",
                json!([{"commitment":"confirmed", "minContextSlot":min_slot}]),
            )
            .await
        })
    }
}

impl<C: json_rpc::Client> Client<C> {
    /// Where `transaction` landed, searching the node's whole history; `None`
    /// when the node has not seen it.
    ///
    /// # Errors
    ///
    /// [`crate::native::ErrorKind::RpcUnavailable`], also for a status that
    /// contradicts itself or lies outside `min_slot..=` the reply's slot.
    pub async fn signature_status(
        &self,
        transaction: &TransactionId,
        min_slot: u64,
    ) -> Result<Reading<Option<SignatureStatus>>, Error> {
        let method = "getSignatureStatuses";
        let context = self
            .contextual::<Vec<Option<StatusWire>>>(
                method,
                json!([[transaction.to_string()], {"searchTransactionHistory":true}]),
                min_slot,
            )
            .await?;
        let [status] = <[Option<StatusWire>; 1]>::try_from(context.value)
            .map_err(|_| Error::malformed(method))?;
        let status = status
            .map(|status| {
                status
                    .coherent(min_slot, context.slot)
                    .ok_or_else(|| Error::malformed(method))
            })
            .transpose()?;
        Ok(Reading {
            slot: context.slot,
            value: status,
        })
    }
}

impl StatusWire {
    /// The status, unless it lies outside `min_slot..=slot`, names an unknown
    /// commitment, or claims confirmations after finalization.
    fn coherent(self, min_slot: u64, slot: u64) -> Option<SignatureStatus> {
        let known = matches!(
            self.confirmation_status.as_str(),
            "processed" | "confirmed" | "finalized"
        );
        let finalized = self.confirmation_status == "finalized";
        if self.slot < min_slot
            || self.slot > slot
            || !known
            || finalized && self.confirmations.is_some()
        {
            return None;
        }
        Some(SignatureStatus {
            slot: self.slot,
            failed: self.err.is_some(),
        })
    }
}
