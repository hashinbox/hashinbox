//! [`native::Error`] into each SDK contract's error type, and the two
//! configuration errors this chain meets while composing itself back into it.
//!
//! Messages and `retryable` are preserved; only the classification changes.

use base::{TransactionError, TransactionErrorKind};
use indexing::{IndexError, SourceError};
use wallets::{SendError, WalletErrorKind};

use crate::native::{self, ErrorKind, TransactionId};

/// A value `sdk/config` refused: an unusable endpoint list, contradictory
/// repetition bounds, or a zero cadence bound. Every one of them is this
/// deployment's configuration.
impl From<sdk_config::Error> for native::Error {
    fn from(error: sdk_config::Error) -> Self {
        Self::new(ErrorKind::InvalidConfiguration, error.message)
    }
}

/// What the index reports while [`crate::Node::compose`] builds it. The
/// classification is already made; only the name of the class changes.
impl From<IndexError> for native::Error {
    fn from(error: IndexError) -> Self {
        let kind = match error.kind {
            indexing::IndexErrorKind::InvalidRequest => ErrorKind::InvalidConfiguration,
            indexing::IndexErrorKind::NetworkMismatch => ErrorKind::Divergent,
            indexing::IndexErrorKind::InvalidBlock => ErrorKind::InvalidBlock,
            _ => ErrorKind::Other,
        };
        Self {
            kind,
            message: error.message,
            retryable: error.retryable,
        }
    }
}

impl From<native::Error> for SourceError {
    fn from(error: native::Error) -> Self {
        Self::new(error.message, error.retryable)
    }
}

impl From<native::Error> for IndexError {
    /// Invalid retained evidence is terminal; everything else is a source failure.
    fn from(error: native::Error) -> Self {
        match error.kind {
            ErrorKind::InvalidBlock => Self::invalid_block(error.message),
            _ => SourceError::from(error).into(),
        }
    }
}

impl From<native::Error> for TransactionError {
    /// A preparation failure: nothing was submitted, so no transaction ID.
    fn from(error: native::Error) -> Self {
        let kind = match error.kind {
            ErrorKind::InvalidAddress => TransactionErrorKind::InvalidAddress,
            ErrorKind::InvalidTransaction | ErrorKind::InvalidBlock => {
                TransactionErrorKind::InvalidTransaction
            }
            ErrorKind::InvalidConfiguration | ErrorKind::Unsupported => {
                TransactionErrorKind::Unsupported
            }
            ErrorKind::InsufficientFunds => TransactionErrorKind::InsufficientFunds,
            ErrorKind::FeeUnavailable => TransactionErrorKind::Fee,
            ErrorKind::RpcUnavailable => TransactionErrorKind::Unavailable,
            ErrorKind::Divergent => TransactionErrorKind::Divergent,
            ErrorKind::Signer | ErrorKind::InvalidSecret | ErrorKind::KeyGeneration => {
                TransactionErrorKind::Signing
            }
            ErrorKind::Rejected => TransactionErrorKind::Rejected,
            ErrorKind::Other => TransactionErrorKind::Unknown,
        };
        Self::new(kind, error.message)
    }
}

impl From<native::Error> for wallets::WalletError {
    fn from(error: native::Error) -> Self {
        let kind = match error.kind {
            ErrorKind::InvalidAddress => WalletErrorKind::InvalidAddress,
            ErrorKind::InvalidSecret => WalletErrorKind::InvalidSecret,
            ErrorKind::KeyGeneration => WalletErrorKind::Generation,
            ErrorKind::InvalidConfiguration | ErrorKind::Unsupported => {
                WalletErrorKind::Unsupported
            }
            ErrorKind::RpcUnavailable | ErrorKind::InvalidBlock | ErrorKind::Other => {
                WalletErrorKind::Unavailable
            }
            ErrorKind::InvalidTransaction
            | ErrorKind::InsufficientFunds
            | ErrorKind::FeeUnavailable
            | ErrorKind::Divergent
            | ErrorKind::Signer
            | ErrorKind::Rejected => WalletErrorKind::Transaction,
        };
        Self::new(kind, error.message)
    }
}

/// The chain's native error into the single-transaction contract's error.
///
/// Solana's batch pipeline settles through `wallets::SendError` and this
/// module's `send_failure`; these three are the same mapping for a
/// caller that drives `native` directly (`docs/CHAIN_LAYOUT.md` section 1).
impl native::Error {
    /// A failed submission of `id`: [`ErrorKind::RpcUnavailable`] means the
    /// attempt may have reached the node, so the error is
    /// [`Self::ambiguous`]; any other kind failed before the wire.
    #[must_use]
    pub fn submission(self, id: &TransactionId) -> TransactionError {
        match self.kind {
            ErrorKind::RpcUnavailable => self.ambiguous(id),
            _ => self.into(),
        }
    }

    /// An unknown outcome of `id`: `Unknown` (Solana's documented
    /// unknown-outcome kind, `docs/CHAIN_LAYOUT.md` section 7), carrying the
    /// local ID of the exact envelope the caller holds.
    #[must_use]
    pub fn ambiguous(self, id: &TransactionId) -> TransactionError {
        TransactionError::new(TransactionErrorKind::Unknown, self.message)
            .with_ambiguous_transaction_id(id.into())
    }

    /// A refusal before anything was submitted: `Unavailable` when a retry
    /// may succeed, otherwise `Rejected`. Never ambiguous.
    #[must_use]
    pub fn definite(self) -> TransactionError {
        let kind = if self.retryable {
            TransactionErrorKind::Unavailable
        } else {
            TransactionErrorKind::Rejected
        };
        TransactionError::new(kind, self.message)
    }
}

/// A failed send as the single-transaction contracts (`TransactionBuilder`,
/// `Broadcaster`) report it. An unknown outcome keeps its local ID and is
/// [`TransactionErrorKind::Unknown`].
pub(crate) fn send_failure(error: SendError) -> TransactionError {
    let kind = match error.source.kind {
        _ if error.ambiguous_transaction_id.is_some() => TransactionErrorKind::Unknown,
        WalletErrorKind::InvalidAddress | WalletErrorKind::AddressMismatch => {
            TransactionErrorKind::InvalidAddress
        }
        WalletErrorKind::InvalidAmount => TransactionErrorKind::InvalidAmount,
        WalletErrorKind::Unsupported => TransactionErrorKind::Unsupported,
        WalletErrorKind::Generation => TransactionErrorKind::Signing,
        WalletErrorKind::Unavailable | WalletErrorKind::SourceBusy => {
            TransactionErrorKind::Unavailable
        }
        _ => TransactionErrorKind::InvalidTransaction,
    };
    let converted = TransactionError::new(kind, error.source.message);
    match error.ambiguous_transaction_id {
        Some(id) => converted.with_ambiguous_transaction_id(id),
        None => converted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_and_index_errors_keep_message_and_retryability() {
        for retryable in [false, true] {
            let source = SourceError::from(native::Error::rpc("node busy", retryable));
            assert_eq!(
                (source.message.as_str(), source.retryable),
                ("node busy", retryable)
            );
            let index = IndexError::from(native::Error::rpc("node busy", retryable));
            assert_eq!(index.kind, indexing::IndexErrorKind::Source);
            assert_eq!(index.retryable, retryable);
        }
        let invalid = IndexError::from(native::Error::invalid_block("bad signature"));
        assert_eq!(invalid.kind, indexing::IndexErrorKind::InvalidBlock);
        assert!(!invalid.retryable);
    }

    #[test]
    fn preparation_conversion_preserves_kind_message_and_absence_of_ambiguity() {
        for (native, expected) in [
            (
                ErrorKind::InvalidAddress,
                TransactionErrorKind::InvalidAddress,
            ),
            (
                ErrorKind::InvalidTransaction,
                TransactionErrorKind::InvalidTransaction,
            ),
            (ErrorKind::Unsupported, TransactionErrorKind::Unsupported),
            (ErrorKind::RpcUnavailable, TransactionErrorKind::Unavailable),
            (ErrorKind::Signer, TransactionErrorKind::Signing),
            (
                ErrorKind::InsufficientFunds,
                TransactionErrorKind::InsufficientFunds,
            ),
            (ErrorKind::Rejected, TransactionErrorKind::Rejected),
            (ErrorKind::Divergent, TransactionErrorKind::Divergent),
        ] {
            let mapped = TransactionError::from(native::Error::new(native, "terminal"));
            assert_eq!(
                (mapped.kind, mapped.message.as_str()),
                (expected, "terminal")
            );
            assert_eq!(mapped.ambiguous_transaction_id, None);
        }
    }

    #[test]
    fn wallet_errors_classify_keys_addresses_and_unsupported_shapes() {
        for (native, expected) in [
            (ErrorKind::InvalidSecret, WalletErrorKind::InvalidSecret),
            (ErrorKind::KeyGeneration, WalletErrorKind::Generation),
            (ErrorKind::InvalidAddress, WalletErrorKind::InvalidAddress),
            (ErrorKind::Unsupported, WalletErrorKind::Unsupported),
            (ErrorKind::RpcUnavailable, WalletErrorKind::Unavailable),
            (ErrorKind::InsufficientFunds, WalletErrorKind::Transaction),
        ] {
            let mapped = wallets::WalletError::from(native::Error::new(native, "message"));
            assert_eq!(
                (mapped.kind, mapped.message.as_str()),
                (expected, "message")
            );
        }
    }

    #[test]
    fn only_an_unknown_send_outcome_carries_the_local_id() {
        let id = base::TransactionId::new("local");
        let mut source = wallets::WalletError::new(WalletErrorKind::Unavailable, "unknown outcome");
        source.ambiguous_transaction_id = Some(id.clone());
        let unknown = send_failure(SendError::item(0, Vec::new(), source));
        assert_eq!(
            (unknown.kind, unknown.ambiguous_transaction_id),
            (TransactionErrorKind::Unknown, Some(id))
        );
        for (kind, expected) in [
            (
                WalletErrorKind::SourceBusy,
                TransactionErrorKind::Unavailable,
            ),
            (
                WalletErrorKind::Unsupported,
                TransactionErrorKind::Unsupported,
            ),
            (
                WalletErrorKind::Transaction,
                TransactionErrorKind::InvalidTransaction,
            ),
            (
                WalletErrorKind::AddressMismatch,
                TransactionErrorKind::InvalidAddress,
            ),
        ] {
            let definite = send_failure(SendError::definite(kind, "definite"));
            assert_eq!(
                (
                    definite.kind,
                    definite.message.as_str(),
                    definite.ambiguous_transaction_id
                ),
                (expected, "definite", None)
            );
        }
    }

    /// Every variant this chain defines. The exhaustive `match` in each
    /// conversion above forces a new variant to be classified; this list
    /// forces it to be covered by the parity test below.
    const EVERY_KIND: &[ErrorKind] = &[
        ErrorKind::InvalidAddress,
        ErrorKind::InvalidTransaction,
        ErrorKind::InvalidBlock,
        ErrorKind::InvalidConfiguration,
        ErrorKind::InvalidSecret,
        ErrorKind::KeyGeneration,
        ErrorKind::InsufficientFunds,
        ErrorKind::FeeUnavailable,
        ErrorKind::Unsupported,
        ErrorKind::RpcUnavailable,
        ErrorKind::Divergent,
        ErrorKind::Signer,
        ErrorKind::Rejected,
        ErrorKind::Other,
    ];

    #[test]
    fn every_kind_is_classified_and_no_transaction_kind_is_unreachable() {
        let mut reached = Vec::new();
        for kind in EVERY_KIND {
            let transaction = TransactionError::from(native::Error::new(*kind, "message"));
            assert_eq!(transaction.message, "message", "{kind:?}");
            assert_eq!(transaction.ambiguous_transaction_id, None, "{kind:?}");
            let wallet = wallets::WalletError::from(native::Error::new(*kind, "message"));
            assert_eq!(wallet.message, "message", "{kind:?}");
            assert_eq!(wallet.ambiguous_transaction_id, None, "{kind:?}");
            reached.push(transaction.kind);
        }
        for required in [
            TransactionErrorKind::InvalidAddress,
            TransactionErrorKind::InvalidTransaction,
            TransactionErrorKind::Unsupported,
            TransactionErrorKind::InsufficientFunds,
            TransactionErrorKind::Fee,
            TransactionErrorKind::Signing,
            TransactionErrorKind::Unavailable,
            TransactionErrorKind::Divergent,
            TransactionErrorKind::Rejected,
            TransactionErrorKind::Unknown,
        ] {
            assert!(
                reached.contains(&required),
                "{required:?} is unreachable from this chain"
            );
        }
    }

    #[test]
    fn submission_ambiguous_and_definite_agree_across_chains() {
        let id = TransactionId::from(solana_signature::Signature::from([7; 64]));
        let unknown = native::Error::rpc("outcome unknown", true).submission(&id);
        assert_eq!(unknown.kind, TransactionErrorKind::Unknown);
        assert_eq!(unknown.ambiguous_transaction_id, Some((&id).into()));
        let rejected = native::Error::new(ErrorKind::Rejected, "refused").submission(&id);
        assert_eq!(rejected.kind, TransactionErrorKind::Rejected);
        assert_eq!(rejected.ambiguous_transaction_id, None);
        let forced = native::Error::new(ErrorKind::Rejected, "replay failed").ambiguous(&id);
        assert_eq!(forced.kind, TransactionErrorKind::Unknown);
        assert_eq!(forced.ambiguous_transaction_id, Some((&id).into()));
        for (retryable, expected) in [
            (true, TransactionErrorKind::Unavailable),
            (false, TransactionErrorKind::Rejected),
        ] {
            let definite = native::Error::rpc("claim failed", retryable).definite();
            assert_eq!(definite.kind, expected);
            assert_eq!(definite.ambiguous_transaction_id, None);
        }
    }
}
