use std::{error, fmt};

/// Every failure of Bitcoin protocol code: parsing, RPC, fee policy, signing.
///
/// It names no SDK contract. The adapter's `error.rs` converts it into
/// `indexing` and `wallets` errors, keeping the message and `retryable`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// The failure class; see [`ErrorKind`].
    pub kind: ErrorKind,
    /// Safe to show: never contains node-supplied error text or secrets.
    pub message: String,
    /// Whether repeating the same operation later may succeed.
    pub retryable: bool,
}

/// What went wrong, independent of which SDK contract reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// Not a valid Bitcoin address for the configured network.
    InvalidAddress,
    /// A transfer or signed transaction that cannot be built or accepted.
    InvalidTransaction,
    /// Retained block evidence violates the protocol; retrying cannot fix it.
    InvalidBlock,
    /// Inconsistent network or wallet settings, or an unusable node deployment.
    InvalidConfiguration,
    /// Not a valid secp256k1 secret.
    InvalidSecret,
    /// The operating system could not supply randomness.
    KeyGeneration,
    /// The spendable outputs cannot cover the transfers plus the fee.
    InsufficientFunds,
    /// The fee rate is zero, unavailable, or above the configured ceiling.
    FeeUnavailable,
    /// The node failed or returned something unusable; see `retryable`.
    ///
    /// From [`crate::native::rpc::Submitter::broadcast`] it means the
    /// submission outcome is unknown.
    RpcUnavailable,
    /// The node serves a different network than the one configured.
    Divergent,
    /// The signer failed or returned an unusable signature.
    Signer,
    /// The node definitively rejected the transaction.
    Rejected,
    /// An internal invariant failed.
    Other,
}

impl Error {
    /// A terminal error of `kind`.
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retryable: false,
        }
    }

    /// A node failure with an explicit retry decision.
    #[must_use]
    pub fn rpc(message: impl Into<String>, retryable: bool) -> Self {
        Self {
            kind: ErrorKind::RpcUnavailable,
            message: message.into(),
            retryable,
        }
    }

    /// Retained block evidence that fails validation.
    #[must_use]
    pub fn invalid_block(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidBlock, message)
    }

    /// A transfer or transaction that cannot be built or accepted.
    #[must_use]
    pub fn invalid_transaction(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidTransaction, message)
    }

    /// The spendable outputs cannot cover the transfers plus the fee.
    pub(crate) fn insufficient(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InsufficientFunds, message)
    }

    /// The signer failed or returned an unusable signature.
    pub(crate) fn signing(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Signer, message)
    }

    /// A node reply that does not have the documented shape; a lagging or
    /// restarting node may answer correctly later.
    pub(crate) fn invalid_reply(message: impl Into<String>) -> Self {
        Self::rpc(message, true)
    }

    /// Evidence from the node that failed validation; retry later.
    pub(crate) fn unusable(self) -> Self {
        Self::rpc(self.message, true)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl error::Error for Error {}
