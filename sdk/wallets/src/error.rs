use std::{error, fmt};

use base::TransactionId;

/// Chain-neutral wallet failure classes; `payment-server` maps them to HTTP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalletErrorKind {
    /// No factory for the asset, a foreign wallet, or an unsupported request.
    Unsupported,
    /// The wallet id or asset key is already registered.
    Duplicate,
    /// No wallet with this id.
    NotFound,
    /// The index or registry holds conflicting state.
    Conflict,
    /// A node, index, or lock is unavailable; retrying may succeed.
    Unavailable,
    /// The sending address already has a send in flight.
    SourceBusy,
    /// Key generation failed.
    Generation,
    /// Imported secret material is not a valid key.
    InvalidSecret,
    /// Address text or network is wrong.
    InvalidAddress,
    /// The amount is not positive or not representable in the asset.
    InvalidAmount,
    /// The batch is empty or has more than [`crate::MAX_TRANSFERS`] items.
    InvalidBatch,
    /// Source and destination (or a wallet and its key) disagree.
    AddressMismatch,
    /// A definite transaction failure (see `base::TransactionErrorKind`).
    Transaction,
    /// The entry is a watched address without a key: it has history, but no
    /// balance read or send.
    WatchOnly,
    /// The asset's network is paused ([`crate::Wallets::pause`]), for example
    /// while it is rescanned: nothing new starts on it until it resumes.
    Paused,
}

impl WalletErrorKind {
    /// Whether a failure of this kind is transient: the node or the source was
    /// busy or unreachable, or the network paused, and nothing about the
    /// request was wrong. Trying
    /// again is only safe when the failure also accepted nothing and is not
    /// ambiguous; this says nothing about either.
    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(self, Self::Unavailable | Self::SourceBusy | Self::Paused)
    }
}

/// A wallet operation failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalletError {
    /// Stable classification.
    pub kind: WalletErrorKind,
    /// Human-readable detail; never contains key material.
    pub message: String,
    /// Canonical local ID preserved from a concrete transaction-layer ambiguity.
    pub ambiguous_transaction_id: Option<TransactionId>,
}

impl WalletError {
    /// Builds an error with no ambiguous transaction.
    #[must_use]
    pub fn new(kind: WalletErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            ambiguous_transaction_id: None,
        }
    }
}

impl fmt::Display for WalletError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl error::Error for WalletError {}

impl From<base::TransactionError> for WalletError {
    fn from(error: base::TransactionError) -> Self {
        let base::TransactionError {
            kind,
            message,
            ambiguous_transaction_id,
        } = error;
        let kind = match kind {
            base::TransactionErrorKind::InvalidAddress => WalletErrorKind::InvalidAddress,
            base::TransactionErrorKind::InvalidAmount => WalletErrorKind::InvalidAmount,
            base::TransactionErrorKind::Unavailable
            | base::TransactionErrorKind::Unknown
            | base::TransactionErrorKind::Timeout => WalletErrorKind::Unavailable,
            base::TransactionErrorKind::InvalidSnapshot
            | base::TransactionErrorKind::InvalidTransaction
            | base::TransactionErrorKind::Unsupported
            | base::TransactionErrorKind::InsufficientFunds
            | base::TransactionErrorKind::Fee
            | base::TransactionErrorKind::Signing
            | base::TransactionErrorKind::Divergent
            | base::TransactionErrorKind::Rejected => WalletErrorKind::Transaction,
        };
        Self {
            kind,
            message,
            ambiguous_transaction_id,
        }
    }
}
