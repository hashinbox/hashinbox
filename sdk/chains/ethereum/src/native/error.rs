use std::{error, fmt};

/// Every failure of Ethereum protocol code: parsing, RPC, fee policy, signing.
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
    /// Not a valid Ethereum address.
    InvalidAddress,
    /// A transfer or signed transaction that cannot be built or accepted.
    InvalidTransaction,
    /// Retained block evidence violates the protocol; retrying cannot fix it.
    InvalidBlock,
    /// Inconsistent network, fee limits, or wallet settings.
    InvalidConfiguration,
    /// Not a valid secp256k1 secret.
    InvalidSecret,
    /// The operating system could not supply randomness.
    KeyGeneration,
    /// The balance cannot cover value plus the worst-case fee.
    InsufficientFunds,
    /// Gas or fee quotes exceed configured ceilings.
    FeeUnavailable,
    /// The node failed or returned something unusable; see `retryable`.
    RpcUnavailable,
    /// The node serves a different chain than the one configured.
    Divergent,
    /// The signer failed or returned an unusable signature.
    Signer,
    /// The node or EVM definitively rejected the transaction.
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

    /// Retained block or receipt evidence that fails validation.
    #[must_use]
    pub fn invalid_block(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidBlock, message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl error::Error for Error {}
