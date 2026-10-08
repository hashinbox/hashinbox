use std::{error, fmt};

/// Every failure of Solana protocol code: parsing, RPC, fees, signing.
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
    /// Not canonical Base58 of exactly 32 bytes.
    InvalidAddress,
    /// A transfer or message that cannot be built, signed, or encoded.
    InvalidTransaction,
    /// Retained block evidence violates the protocol; retrying cannot fix it.
    InvalidBlock,
    /// Inconsistent network, RPC, or wallet settings, or a node deployment
    /// without the executable Memo-v3 program.
    InvalidConfiguration,
    /// Not a 32-byte Ed25519 seed.
    InvalidSecret,
    /// The operating system could not supply randomness.
    KeyGeneration,
    /// A source cannot pay its transfers plus their fees.
    InsufficientFunds,
    /// The node did not price the message, so there is no acceptable fee.
    FeeUnavailable,
    /// A destination or account shape this native SOL integration cannot pay.
    Unsupported,
    /// The node failed, answered below a requested context slot, or returned
    /// something unusable; see `retryable`.
    ///
    /// From [`crate::native::rpc::Submitter::broadcast`] it means the
    /// submission outcome is unknown.
    RpcUnavailable,
    /// The node serves a cluster with another genesis hash.
    Divergent,
    /// The signer failed or returned a signature that does not verify.
    Signer,
    /// The node's simulation rejected the signed transaction.
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

    /// A node reply of `method` that does not have the documented shape; a
    /// lagging or restarting node may answer correctly later.
    #[must_use]
    pub fn malformed(method: &str) -> Self {
        Self::rpc(format!("Solana RPC {method} returned malformed data"), true)
    }

    /// A contextual reply older than the slot it was asked to reach.
    #[must_use]
    pub fn below_min_slot() -> Self {
        Self::rpc(
            "Solana RPC response is below its requested context floor",
            true,
        )
    }

    /// Evidence from the node that failed validation; retry later.
    #[must_use]
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
