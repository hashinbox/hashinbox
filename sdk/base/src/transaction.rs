use std::{collections::BTreeSet, error::Error as StdError, fmt, pin::Pin};

use serde::{Deserialize, Serialize};

use crate::{Address, Addressed, Asset, Coin, Decimal, Network};

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
/// knows nothing of broadcasting: save the signed transactions, then hand each
/// to a [`Broadcaster`].
pub trait Builder: Send {
    /// Queues a transfer of `amount` (atomic units) `to` a recipient;
    /// checks the address and amount now, without I/O.
    ///
    /// # Errors
    ///
    /// `Failed` for an invalid address or amount; `Unsupported` for a
    /// transfer this builder cannot add.
    fn transfer(&mut self, to: Address, amount: Decimal) -> Result<(), Error>;

    /// Fetches its own chain inputs (coins, nonces or blockhash), estimates
    /// fees and signs every queued transfer, in order: one or more
    /// [`State::Pending`] transactions with their legs and envelopes, each
    /// built on the ones before it. Consumes the builder, so it cannot sign
    /// twice. Nothing leaves the process.
    ///
    /// # Errors
    ///
    /// `Failed` for no transfers; any build, fee, funds, or signing failure.
    fn sign<'a>(self: Box<Self>) -> Future<'a, Result<Vec<Transaction>, Error>>
    where
        Self: 'a;
}

/// The only external effect in transaction submission: the exact signed
/// bytes to every configured node.
pub trait Broadcaster: Send + Sync {
    /// Submits the exact envelope to every configured node.
    ///
    /// # Errors
    ///
    /// `Rejected` is definite (the node refused); `Expired` means the bytes
    /// can no longer land and were not seen; `Unavailable`, `Unknown` and
    /// `Timeout` are transient: retry with the same bytes.
    fn broadcast<'a>(
        &'a self,
        transaction: &'a (dyn Enveloped + Sync),
    ) -> Future<'a, Result<(), Error>>;
}

/// Anything carrying a signed transaction in its chain's own encoding.
pub trait Enveloped {
    /// The signed transaction, exactly as the chain's node accepts it.
    fn envelope(&self) -> &[u8];
}

impl Enveloped for [u8] {
    fn envelope(&self) -> &[u8] {
        self
    }
}

impl Enveloped for Vec<u8> {
    fn envelope(&self) -> &[u8] {
        self
    }
}

/// Transactions, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Transactions(pub Vec<Transaction>);

impl Transactions {
    /// Every leg with its transaction and index, in order.
    pub fn legs(&self) -> impl Iterator<Item = (&Id, u32, &Leg)> {
        self.0.iter().flat_map(|transaction| {
            (0_u32..)
                .zip(&transaction.legs)
                .map(move |(index, leg)| (&transaction.id, index, leg))
        })
    }

    /// Every address a leg names.
    #[must_use]
    pub fn addresses(&self) -> BTreeSet<&Address> {
        self.legs().map(|(_, _, leg)| leg.address()).collect()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<'a> IntoIterator for &'a Transactions {
    type Item = &'a Transaction;
    type IntoIter = std::slice::Iter<'a, Transaction>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Where a transaction stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Broadcast or about to be, not yet mined (in the mempool).
    Pending,
    /// Executed.
    Succeeded,
    /// Mined but failed; only its fee moved.
    Reverted(Option<Reason>),
}

/// A transaction and its legs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    /// Its identity.
    pub id: Id,
    /// How it executed.
    pub state: State,
    /// Its position in the block; `None` until mined.
    pub order: Option<u64>,
    /// Its legs; a leg's identity is `(id, its index here)`.
    pub legs: Vec<Leg>,
    /// The signed transaction in its chain's own encoding.
    pub bytes: Vec<u8>,
}

impl Enveloped for Transaction {
    fn envelope(&self) -> &[u8] {
        &self.bytes
    }
}

impl Transacted for Transaction {
    fn txid(&self) -> &Id {
        &self.id
    }
}

/// One side of a movement inside a transaction. Amounts are atomic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Leg {
    /// Value arrived at `address`.
    Received {
        /// The recipient.
        address: Address,
        /// The asset.
        asset: Asset,
        /// Atomic amount.
        amount: Decimal,
    },
    /// Value left `address`.
    Spent {
        /// The sender.
        address: Address,
        /// The asset.
        asset: Asset,
        /// Atomic amount.
        amount: Decimal,
        /// The UTXO consumed, on UTXO chains.
        coin: Option<Coin>,
    },
    /// Value of `address` destroyed, with no recipient.
    Burned {
        /// The holder.
        address: Address,
        /// The asset.
        asset: Asset,
        /// Atomic amount.
        amount: Decimal,
    },
    /// Value created for `address`, with no sender.
    Minted {
        /// The recipient.
        address: Address,
        /// The asset.
        asset: Asset,
        /// Atomic amount.
        amount: Decimal,
    },
    /// The fee `address` paid.
    Fee {
        /// The payer.
        address: Address,
        /// The native asset.
        asset: Asset,
        /// Atomic amount.
        amount: Decimal,
    },
}

impl crate::Networked for Leg {
    fn network(&self) -> &crate::Network {
        &self.address().network
    }
}

impl Addressed for Leg {
    fn address(&self) -> &Address {
        match self {
            Self::Received { address, .. }
            | Self::Spent { address, .. }
            | Self::Burned { address, .. }
            | Self::Minted { address, .. }
            | Self::Fee { address, .. } => address,
        }
    }
}

/// Where transactions are kept.
pub trait Store: Send + Sync {
    /// Keeps `transaction`, such as a pending one before broadcast.
    ///
    /// # Errors
    ///
    /// When the store cannot write.
    fn insert<'a>(&'a self, transaction: &'a Transaction) -> Future<'a, anyhow::Result<()>>;

    /// Every kept transaction with a leg of `address`.
    ///
    /// # Errors
    ///
    /// When the store cannot read.
    fn find_for_address<'a>(
        &'a self,
        address: &'a Address,
    ) -> Future<'a, anyhow::Result<Vec<Transaction>>>;
}
