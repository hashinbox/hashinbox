use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error as StdError,
    fmt,
    pin::Pin,
    sync::Arc,
};

use crypto::{Curve, SecretBytes};
use serde::{Deserialize, Serialize};

use crate::{Address, Addressed, Asset, Coin, Decimal, Fee, Network, Wallet};

/// The future transaction capabilities return: boxed so the traits stay `dyn`-safe.
pub type Future<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// The transaction failures a caller branches on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// A definite failure: an invalid address, amount or transaction,
    /// insufficient funds, no acceptable fee, a signer or node mismatch.
    Failed,
    /// The builder or configuration does not support this asset or
    /// transfer.
    Unsupported,
    /// The node is unreachable, timed out, or the submission outcome is
    /// unknown; retrying may succeed.
    Unavailable,
    /// The node definitively rejected the transaction; see [`Error::reason`].
    Rejected,
    /// The transaction can no longer land (Solana: its blockhash expired)
    /// and was not seen; its outcome is unknown.
    Expired,
    /// The fee it needs is above the chain's fee cap (its configured
    /// ceilings): signing it would pay more than allowed.
    FeeCap,
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

    /// A definite failure, described by `error` (with its causes).
    #[must_use]
    pub fn failed(error: impl fmt::Display) -> Self {
        Self::new(ErrorKind::Failed, format!("{error:#}"))
    }

    /// A transient failure, described by `error` (with its causes); retrying
    /// may succeed.
    #[must_use]
    pub fn unavailable(error: impl fmt::Display) -> Self {
        Self::new(ErrorKind::Unavailable, format!("{error:#}"))
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

/// Who spends: an address and the key that signs for it. Cheap to clone
/// (the secret is shared, zeroized on drop); `Debug` shows no secret.
#[derive(Clone)]
pub struct Signer {
    /// The spending address.
    pub address: Address,
    /// The key's curve.
    pub curve: Curve,
    secret: Arc<SecretBytes>,
}

impl Signer {
    /// The signer of `address` with `secret` on `curve`; the chain checks
    /// that the key derives the address when it signs.
    #[must_use]
    pub fn new(address: Address, curve: Curve, secret: SecretBytes) -> Self {
        Self {
            address,
            curve,
            secret: Arc::new(secret),
        }
    }

    /// The key bytes, in the chain's import format.
    #[must_use]
    pub fn secret(&self) -> &SecretBytes {
        &self.secret
    }
}

impl fmt::Debug for Signer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Signer")
            .field("address", &self.address)
            .field("curve", &self.curve)
            .finish_non_exhaustive()
    }
}

impl<W: Wallet + ?Sized> From<&W> for Signer {
    fn from(wallet: &W) -> Self {
        let (curve, secret) = wallet.private_key();
        Self::new(wallet.address().clone(), curve, secret)
    }
}

impl From<&Arc<dyn Wallet>> for Signer {
    fn from(wallet: &Arc<dyn Wallet>) -> Self {
        Self::from(&**wallet)
    }
}

/// One payment: `amount` (atomic units) `to` a recipient, from its own
/// source or, without one, the builder's default source.
#[derive(Clone, Debug)]
pub struct Transfer {
    /// The recipient.
    pub to: Address,
    /// Atomic amount.
    pub amount: Decimal,
    /// Who pays; `None` for the builder's default source.
    pub from: Option<Signer>,
}

impl Transfer {
    /// `amount` to `to`, from the builder's default source.
    #[must_use]
    pub const fn new(to: Address, amount: Decimal) -> Self {
        Self {
            to,
            amount,
            from: None,
        }
    }

    /// The same transfer, paid by `source`.
    #[must_use]
    pub fn from(mut self, source: impl Into<Signer>) -> Self {
        self.from = Some(source.into());
        self
    }
}

/// One transfer of one asset, signed into one transaction: a builder pays
/// exactly one payment. The asset is fixed when the factory or wallet
/// creates the builder; the transfer is paid by its own source or the
/// default one. What it chose when it signed is its state, saved, so a
/// stuck payment is replaced with the same calls that sent it:
///
/// ```text
/// a payment:  from_transfer(transfer), with_fee(fee), sign(), save()
/// its bump:   load(&state), with_fee(Fee::Times(1.1)), sign()
/// ```
///
/// It knows nothing of broadcasting: save the signed transaction, then
/// hand it to a [`Broadcaster`].
pub trait Builder: Send {
    /// Source used when the transfer has no `from` of its own.
    fn with_default_source(&mut self, source: Signer);

    /// A new payment, `transfer`, its address and amount checked before any
    /// I/O; its slot (coins or nonce) is picked when signing.
    ///
    /// # Errors
    ///
    /// `Failed` for an invalid address or amount.
    #[allow(
        clippy::wrong_self_convention,
        reason = "loads the builder from a transfer; the builder is the receiver"
    )]
    fn from_transfer(&mut self, transfer: Transfer) -> Result<(), Error>;

    /// Signs at `fee`'s rate; [`Fee::default`] until set.
    fn with_fee(&mut self, fee: Fee);

    /// Fetches its chain inputs (coins, nonce or blockhash; a loaded state
    /// keeps its own slot) and the network's rate now, applies the fee's
    /// rule (to it and the state's rate), raises it to the chain's floor,
    /// prices the exact size and signs: one [`State::Pending`] transaction
    /// with its legs and envelope. Nothing leaves the process. Keeps what it
    /// chose as its state: a next sign continues it.
    ///
    /// # Errors
    ///
    /// `Failed` with nothing to sign, a transfer without a source, a key
    /// that does not sign for its address; any build, fee, funds, or signing
    /// failure. `FeeCap` above the chain's fee cap.
    fn sign(&mut self) -> Future<'_, Result<Transaction, Error>>;

    /// What it chose for the last sign: enough to sign the same payment
    /// again, in the same slot. Never a key: the source is its address, its
    /// key the one the builder signs with then.
    ///
    /// # Errors
    ///
    /// `Failed` before anything was signed.
    fn save(&self) -> Result<serde_json::Value, Error>;

    /// A state [`Self::save`] made: the next sign continues it (same nonce
    /// or coins), at a rate never below the saved one.
    ///
    /// # Errors
    ///
    /// `Failed` for a state this chain's builder did not save.
    fn load(&mut self, state: &serde_json::Value) -> Result<(), Error>;
}

/// The only external effect in transaction submission: a signed
/// transaction's exact bytes to every configured node.
///
/// The SDK does not track what is in flight. A caller that sends money:
///
/// 1. records the transaction (its legs) before broadcasting it, and counts
///    its outgoing legs as spent from then on;
/// 2. builds, records and broadcasts a sender's transactions one at a time,
///    one payment each, since coin selection and nonces see only what is
///    recorded;
/// 3. on `Rejected` or `Expired` from a broadcast forgets the record; on a
///    transient error keeps it and broadcasts the same bytes again, never
///    building anew;
/// 4. forgets a pending record only once the chain proves it can never
///    mine (its [`crate::TransactionIndex`]): no clock frees its funds.
pub trait Broadcaster: Send + Sync {
    /// Submits `transaction.bytes` to every configured node.
    ///
    /// # Errors
    ///
    /// `Rejected` when a node refused it (it will not land); `Expired` when
    /// it can no longer land; `Unavailable` when no node answered (it may
    /// have landed): broadcast the same bytes again.
    fn broadcast<'a>(&'a self, transaction: &'a Transaction) -> Future<'a, Result<(), Error>>;
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
}

impl From<Vec<Transaction>> for Transactions {
    fn from(transactions: Vec<Transaction>) -> Self {
        Self(transactions)
    }
}

impl std::ops::Deref for Transactions {
    type Target = [Transaction];

    fn deref(&self) -> &[Transaction] {
        &self.0
    }
}

impl IntoIterator for Transactions {
    type Item = Transaction;
    type IntoIter = std::vec::IntoIter<Transaction>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
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
    /// The last block height it can land at (Solana: its blockhash's last
    /// valid block height); `None`: it never expires.
    pub expires_height: Option<u64>,
}

impl Transaction {
    /// Each (address, asset) it takes from, with what it takes net: the sum
    /// of that pair's signed legs, negative (change coming back is netted
    /// out). What a caller counts as spent while it is pending.
    #[must_use]
    pub fn debits(&self) -> BTreeMap<(Address, Asset), Decimal> {
        let mut sums = BTreeMap::<(Address, Asset), Decimal>::new();
        for leg in &self.legs {
            let pair = (leg.address().clone(), leg.asset().clone());
            *sums.entry(pair).or_insert_with(Decimal::zero) += leg.signed();
        }
        sums.retain(|_, sum| *sum < Decimal::zero());
        sums
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
        /// The UTXO it creates, on UTXO chains.
        coin: Option<Coin>,
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

impl Leg {
    /// The asset it moves.
    #[must_use]
    pub const fn asset(&self) -> &Asset {
        match self {
            Self::Received { asset, .. }
            | Self::Spent { asset, .. }
            | Self::Burned { asset, .. }
            | Self::Minted { asset, .. }
            | Self::Fee { asset, .. } => asset,
        }
    }

    /// Its atomic amount, unsigned.
    #[must_use]
    pub const fn amount(&self) -> &Decimal {
        match self {
            Self::Received { amount, .. }
            | Self::Spent { amount, .. }
            | Self::Burned { amount, .. }
            | Self::Minted { amount, .. }
            | Self::Fee { amount, .. } => amount,
        }
    }

    /// What it does to its address's balance: credits (received, minted)
    /// count up, debits (spent, burned, fees) down.
    #[must_use]
    pub fn signed(&self) -> Decimal {
        match self {
            Self::Received { amount, .. } | Self::Minted { amount, .. } => amount.clone(),
            Self::Spent { amount, .. } | Self::Burned { amount, .. } | Self::Fee { amount, .. } => {
                Decimal::zero() - amount.clone()
            }
        }
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
