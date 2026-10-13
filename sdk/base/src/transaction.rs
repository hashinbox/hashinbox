use std::{error::Error as StdError, fmt, pin::Pin};

use serde::{Deserialize, Serialize};

use crate::{Address, Decimal, Network};

/// The future transaction capabilities return: boxed so the traits stay `dyn`-safe.
pub type Future<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// The transaction failures a caller branches on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// A definite failure: an invalid address, amount or transaction,
    /// insufficient funds, no acceptable fee, a signer or node mismatch.
    Failed,
    /// The builder or configuration does not support this transfer; a
    /// builder refusing one more transfer this way ends its transaction.
    Unsupported,
    /// The node is unreachable, timed out, or the submission outcome is
    /// unknown; retrying may succeed.
    Unavailable,
    /// The node definitively rejected the transaction; see [`Error::reason`].
    Rejected,
    /// The transaction can no longer land (Solana: its blockhash expired)
    /// and was not seen; its outcome is unknown.
    Expired,
}

impl ErrorKind {
    /// Whether retrying the same operation may succeed.
    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(self, Self::Unavailable)
    }
}

/// A failed transaction build, sign, or broadcast.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// Stable classification.
    pub kind: ErrorKind,
    /// Human-readable detail.
    pub message: String,
    /// Why the network refused the transaction, as the chain mapped it; set
    /// on `Rejected` and `Expired`.
    pub reason: Option<Reason>,
}

/// Why a transaction was refused or reverted, as each chain maps its own
/// errors; stored as JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// The sender cannot cover the amount and fee.
    InsufficientFunds,
    /// The sequence is taken: an Ethereum nonce too low or used, Bitcoin
    /// inputs already spent.
    NonceUsed,
    /// The fee is below what the network accepts.
    FeeTooLow,
    /// The transaction can no longer land (Solana: its blockhash expired).
    Expired,
    /// Malformed, or a bad signature.
    Invalid,
    /// Executed and failed (an Ethereum revert, a Solana instruction error).
    Reverted,
    /// Anything else, in the chain's own words.
    Other(String),
}

impl Error {
    /// Builds an error of `kind`.
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            reason: None,
        }
    }

    /// Records why the network refused it.
    #[must_use]
    pub fn with_reason(mut self, reason: Reason) -> Self {
        self.reason = Some(reason);
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for Error {}

/// Network-qualified transaction identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id {
    /// The complete chain/network namespace in which `id` is canonical.
    pub network: Network,
    /// The chain-native transaction id.
    pub id: String,
}

impl crate::Networked for Id {
    fn network(&self) -> &Network {
        &self.network
    }
}

/// Anything that belongs to one transaction.
pub trait Transacted {
    /// The transaction id.
    fn txid(&self) -> &Id;
}

impl Id {
    /// Takes the canonical text of `network`'s chain as the id's identity.
    ///
    /// Text, not anything printable, for the reason [`crate::Address::new`]
    /// states: a chain writes `native_id.to_string()` in its `identity.rs`.
    #[must_use]
    pub fn new(network: Network, id: impl Into<String>) -> Self {
        Self {
            network,
            id: id.into(),
        }
    }

    /// Whether this transaction belongs to `network`.
    #[must_use]
    pub fn belongs_to(&self, network: &Network) -> bool {
        &self.network == network
    }
}

/// Ordered transfers of one asset from one wallet, signed into one or more
/// transactions. The asset is fixed when the wallet creates the builder; it
/// knows nothing of broadcasting: save the signed bytes, then hand each to a
/// [`Broadcaster`].
pub trait Builder: Send {
    /// Queues a transfer of `amount` (display units) `to` a recipient;
    /// checks the address and amount now, without I/O.
    ///
    /// # Errors
    ///
    /// `Failed` for an invalid address or amount; `Unsupported` for a
    /// transfer this builder cannot add.
    fn transfer(&mut self, to: Address, amount: Decimal) -> Result<(), Error>;

    /// Fetches its own chain inputs (coins, nonces or blockhash), estimates
    /// fees and signs every queued transfer, in order: the exact signed
    /// bytes of one or more transactions, each built on the ones before it.
    /// Nothing leaves the process.
    ///
    /// # Errors
    ///
    /// `Failed` for no transfers; any build, fee, funds, or signing failure.
    fn sign(&mut self) -> Future<'_, Result<Vec<Vec<u8>>, Error>>;
}

/// The only external effect in transaction submission: the exact signed
/// bytes to every configured node.
pub trait Broadcaster: Send + Sync {
    /// Submits the exact signed bytes to every configured node.
    ///
    /// # Errors
    ///
    /// `Rejected` is definite (the node refused); `Expired` means the bytes
    /// can no longer land and were not seen; `Unavailable`, `Unknown` and
    /// `Timeout` are transient: retry with the same bytes.
    fn broadcast<'a>(&'a self, bytes: &'a [u8]) -> Future<'a, Result<(), Error>>;
}
