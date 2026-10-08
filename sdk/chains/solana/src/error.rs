//! [`native::Error`] into each SDK contract's error type, and the two
//! configuration errors this chain meets while composing itself back into it.
//!
//! Messages and `retryable` are preserved; only the classification changes.

use base::{TransactionError, TransactionErrorKind};
use indexing::{IndexError, SourceError};
use wallets::{SendError, WalletErrorKind};

use crate::native::{self, ErrorKind, TransactionId};

/// A value `payments::config` refused: an unusable endpoint list, contradictory
/// repetition bounds, or a zero cadence bound. Every one of them is this
/// deployment's configuration.
impl From<payments::config::Error> for native::Error {
    fn from(error: payments::config::Error) -> Self {
        Self::new(ErrorKind::InvalidConfiguration, error.message)
    }
}

/// What the index reports while [`crate::ChainHandle::compose`] builds it. The
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
/// caller that drives `native` directly.
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
    /// unknown-outcome kind), carrying the
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
