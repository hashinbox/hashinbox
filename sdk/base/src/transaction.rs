use std::{error::Error as StdError, fmt, future::Future, pin::Pin};

use serde::{Deserialize, Serialize};

use crate::{Address, AssetId, Decimal, Network};

#[path = "transaction_claim.rs"]
mod claim;

pub use claim::Claim;

/// The future transaction capabilities return: boxed so the traits stay `dyn`-safe.
pub type TransactionFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Chain-neutral classification of a build, sign, or broadcast failure.
///
/// `wallets::Error` folds `Unavailable`, `Unknown`, and `Timeout` into its
/// retryable `Unavailable`; every other kind is a definite transaction failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// A destination is not a valid address of this network.
    InvalidAddress,
    /// The amount is negative, too precise, or out of range.
    InvalidAmount,
    /// A [`Snapshot`] cannot be restored.
    InvalidSnapshot,
    /// The transaction is malformed or violates a chain rule.
    InvalidTransaction,
    /// The configuration or request is not supported.
    Unsupported,
    /// The balance cannot cover the transfers plus the fee.
    InsufficientFunds,
    /// No acceptable fee: unavailable or above the configured ceiling.
    Fee,
    /// The signer failed.
    Signing,
    /// The node is unreachable or returned something unusable.
    Unavailable,
    /// The node serves a different chain or network than configured.
    Divergent,
    /// The node definitively rejected the transaction.
    Rejected,
    /// The submission outcome is unknown; see [`Error::ambiguous_transaction_id`].
    Unknown,
    /// The operation did not finish in time.
    Timeout,
}

/// A failed transaction build, sign, or broadcast.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// Stable classification.
    pub kind: ErrorKind,
    /// Human-readable detail.
    pub message: String,
    /// Canonical local identifier for reconciling an unknown submission outcome.
    pub ambiguous_transaction_id: Option<Id>,
}

impl Error {
    /// Builds an error with no ambiguous transaction.
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            ambiguous_transaction_id: None,
        }
    }

    /// Marks an unknown submission outcome with its locally derived identity.
    ///
    /// Only the concrete chain transaction layer that derived `transaction_id`
    /// from the exact locally signed envelope may attach it. Higher layers must
    /// preserve the typed value and must not derive it from node output.
    #[must_use]
    pub fn with_ambiguous_transaction_id(mut self, transaction_id: Id) -> Self {
        self.ambiguous_transaction_id = Some(transaction_id);
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for Error {}

/// Canonical, chain-native textual transaction identifier.
///
/// The concrete chain validates and constructs this value. Keeping its native
/// text form makes it identical to the identifier emitted by indexing while
/// preserving each protocol's canonical formatting rules.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Id(String);

impl Id {
    /// Wraps text the chain already validated as canonical.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The canonical text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for Id {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Id {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl fmt::Display for Id {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Network-qualified transaction identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ref {
    /// The complete chain/network namespace in which `id` is canonical.
    pub network: Network,
    /// The chain-native transaction id.
    pub id: Id,
}

impl Ref {
    /// Takes the canonical text of `network`'s chain as the id's identity.
    ///
    /// Text, not anything printable, for the reason [`crate::Address::new`]
    /// states: a chain writes `native_id.to_string()` in its `identity.rs`.
    #[must_use]
    pub fn new(network: Network, id: impl Into<String>) -> Self {
        Self {
            network,
            id: Id::new(id),
        }
    }

    /// Whether this transaction belongs to `network`.
    #[must_use]
    pub fn belongs_to(&self, network: &Network) -> bool {
        &self.network == network
    }
}

/// Exact signed wire bytes; `Debug` redacts them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope(Vec<u8>);

impl Envelope {
    /// Wraps signed wire bytes.
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    /// The bytes to submit.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Envelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Envelope")
            .field("bytes", &"[REDACTED]")
            .field("length", &self.0.len())
            .finish()
    }
}

/// Versioned JSON of an unsigned transaction under construction.
///
/// A chain's builder writes it inside [`Draft`] and can restore itself from
/// it; `kind` names the chain's format.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    version: u16,
    kind: String,
    value: serde_json::Value,
}

impl Snapshot {
    /// The envelope version this crate writes.
    pub const VERSION: u16 = 1;

    /// Wraps a chain-owned `value` of format `kind`.
    #[must_use]
    pub fn new(kind: impl Into<String>, value: serde_json::Value) -> Self {
        Self {
            version: Self::VERSION,
            kind: kind.into(),
            value,
        }
    }

    /// The envelope version it was written with.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// The chain-owned format name.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The chain-owned body.
    #[must_use]
    pub const fn value(&self) -> &serde_json::Value {
        &self.value
    }
}

/// One requested payment of a [`Draft`]: where it goes and how much.
///
/// One word for one concept: the chains' `native::Transfer` and
/// `wallets::Transfer` are the same request in native and in wallet terms, and
/// the recipient is `to` in all three. `destination` survives only on the HTTP
/// wire, where `apps/server` names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transfer {
    /// Canonical recipient address, in the source's network.
    pub to: Address,
    /// Display units of the draft's asset.
    pub amount: Decimal,
}

/// What a transaction will do, before any node is asked: no RPC, no key, no
/// fee and no identity yet.
///
/// It carries the intent in typed form *and* the chain's own durable
/// encoding, so inspecting intent never means parsing a chain format, and
/// persisting `snapshot` still restores the builder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    /// The wallet the transfers are paid from.
    pub source: Address,
    /// The requested payments, in the order they were added.
    pub transfers: Vec<Transfer>,
    /// The asset `transfers` move; the fee asset may differ (a token
    /// transfer pays its fee in the chain's own currency).
    pub asset: AssetId,
    /// The chain's own durable encoding; persist it and
    /// `wallets::TransactionFactory::resume` rebuilds this builder.
    pub snapshot: Snapshot,
}

/// Chain-independent transaction construction used by wallet consumers.
///
/// Add transfers, optionally read the [`draft`](Self::draft) to see what will
/// be paid, then [`prepare`](Self::prepare) the signed bytes and read
/// [`SignedTransaction::fee`] to see what it costs; broadcasting is a separate
/// [`Broadcaster`] step.
///
/// Three methods, and inspection lives inside the two values they return
/// rather than in a fourth: a caller who dislikes the fee simply drops the
/// builder, and nothing has left the process.
pub trait TransactionBuilder: Send {
    /// Adds one transfer of `amount` (display units) `to` a recipient.
    ///
    /// # Errors
    ///
    /// Invalid address or amount; `Unsupported` if the chain allows no more transfers.
    fn transfer(&mut self, to: Address, amount: Decimal) -> Result<(), Error>;

    /// What this transaction would do, with its durable [`Snapshot`] inside.
    ///
    /// # Errors
    ///
    /// The builder is not yet complete or valid.
    fn draft(&self) -> Result<Draft, Error>;

    /// Selects inputs or nonce, estimates the fee, and signs.
    ///
    /// # Errors
    ///
    /// Any build, fee, funds, or signing failure.
    fn prepare<'a>(&'a mut self) -> TransactionFuture<'a, Result<SignedTransaction, Error>>;
}

/// Exact signed transaction ready for submission.
///
/// This is durable data, not a live RPC handle. Persisting it before the
/// external effect allows a retry to broadcast the exact same signed bytes
/// without rebuilding or signing again.
#[derive(Clone, Serialize, Deserialize)]
pub struct SignedTransaction {
    version: u16,
    kind: String,
    id: Id,
    envelope: Envelope,
    #[serde(
        serialize_with = "SignedTransaction::write_fee",
        deserialize_with = "SignedTransaction::read_fee"
    )]
    fee: Decimal,
    /// Deliberately outside the durable form: see [`Self::claim`].
    #[serde(skip)]
    claim: Option<Claim>,
}

impl SignedTransaction {
    /// The envelope version this crate writes.
    pub const VERSION: u16 = 1;

    /// Pairs the locally derived `id` with its exact signed `envelope` and
    /// the `fee` those bytes pay.
    #[must_use]
    pub fn new(kind: impl Into<String>, id: Id, envelope: Envelope, fee: Decimal) -> Self {
        Self {
            version: Self::VERSION,
            kind: kind.into(),
            id,
            envelope,
            fee,
            claim: None,
        }
    }

    /// Records what the outbox held to produce these exact bytes.
    ///
    /// Only the send pipeline that did the holding may attach one; nothing
    /// else can tell the truth about it.
    #[must_use]
    pub fn with_claim(mut self, claim: Claim) -> Self {
        self.claim = Some(claim);
        self
    }

    /// Canonical base-10 text, the one spelling every amount is stored as.
    fn write_fee<S: serde::Serializer>(fee: &Decimal, writer: S) -> Result<S::Ok, S::Error> {
        writer.collect_str(fee)
    }

    fn read_fee<'de, D: serde::Deserializer<'de>>(reader: D) -> Result<Decimal, D::Error> {
        let text = String::deserialize(reader)?;
        text.parse().map_err(serde::de::Error::custom)
    }

    /// The envelope version it was written with.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// The chain-owned format name.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The transaction id, derived locally from the signed bytes.
    #[must_use]
    pub const fn id(&self) -> &Id {
        &self.id
    }

    /// The exact bytes to submit.
    #[must_use]
    pub const fn envelope(&self) -> &Envelope {
        &self.envelope
    }

    /// The network fee these exact bytes pay, in the fee asset's atomic
    /// units.
    ///
    /// The fee asset is the chain's own currency, which is not always the
    /// asset being moved: a token transfer pays its fee in BTC, ETH, or SOL.
    /// It lives on the value rather than in a wrapper so the durable record
    /// is complete: persist a signed transaction and a retry still knows
    /// what it costs.
    #[must_use]
    pub const fn fee(&self) -> &Decimal {
        &self.fee
    }

    /// What signing these bytes took, so a caller holding two of them can
    /// ask [`Claim::collides_with`] whether both can still be broadcast.
    ///
    /// Unlike [`Self::fee`] this is **not** part of the durable form, and is
    /// `None` on a value read back from storage. A claim describes the
    /// resources one live process holds, none of which survive a restart, so
    /// a stored one could only answer the collision question with a lie. A
    /// chain that needs it again reconstructs it from the envelope it owns.
    #[must_use]
    pub fn claim(&self) -> Option<&Claim> {
        self.claim.as_ref()
    }
}

/// Two values are the same signed transaction when they carry the same
/// bytes. A [`Claim`] reports on the process that signed them and is not
/// part of their identity, so a persisted transaction still equals the one
/// it was written from.
impl PartialEq for SignedTransaction {
    fn eq(&self, other: &Self) -> bool {
        (
            self.version,
            &self.kind,
            &self.id,
            &self.envelope,
            &self.fee,
        ) == (
            other.version,
            &other.kind,
            &other.id,
            &other.envelope,
            &other.fee,
        )
    }
}

impl Eq for SignedTransaction {}

impl fmt::Debug for SignedTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SignedTransaction")
            .field("version", &self.version)
            .field("kind", &self.kind)
            .field("id", &self.id)
            .field("envelope", &self.envelope)
            .field("fee", &self.fee)
            .field("claim", &self.claim)
            .finish()
    }
}

/// The only external effect in transaction submission.
pub trait Broadcaster: Send + Sync {
    /// Submits the exact signed bytes.
    ///
    /// # Errors
    ///
    /// `Rejected` is definite; `Unavailable`, `Unknown`, and `Timeout` may
    /// carry [`Error::ambiguous_transaction_id`] when the outcome is unknown.
    fn broadcast<'a>(
        &'a self,
        transaction: &'a SignedTransaction,
    ) -> TransactionFuture<'a, Result<Submission, Error>>;
}

/// A transaction accepted for submission by at least one node.
///
/// Confirmation is observed through indexing and history; wallet code never
/// polls a node waiting for finality.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submission {
    /// The accepted transaction's id.
    pub id: Id,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_uses_display_text_within_its_network() {
        let network = Network::new(crate::ChainName::new("test"), "mainnet");
        let reference = Ref::new(network.clone(), "abc");

        assert_eq!(reference.id.as_str(), "abc");
        assert!(reference.belongs_to(&network));
    }

    #[test]
    fn ordinary_error_has_no_ambiguous_transaction_id() {
        let error = Error::new(ErrorKind::Unavailable, "submission is unavailable");

        assert_eq!(error.kind, ErrorKind::Unavailable);
        assert_eq!(error.message, "submission is unavailable");
        assert_eq!(error.ambiguous_transaction_id, None);
    }

    #[test]
    fn provider_message_cannot_supply_an_ambiguous_transaction_id() {
        let error = Error::new(
            ErrorKind::Unknown,
            "provider claimed transaction canonical-id",
        );

        assert_eq!(error.ambiguous_transaction_id, None);
    }

    #[test]
    fn explicit_ambiguity_preserves_the_typed_transaction_id() {
        let id = Id::new("canonical-id");
        let error = Error::new(ErrorKind::Timeout, "submission outcome is unknown")
            .with_ambiguous_transaction_id(id.clone());

        assert_eq!(error.kind, ErrorKind::Timeout);
        assert_eq!(error.message, "submission outcome is unknown");
        assert_eq!(error.to_string(), "submission outcome is unknown");
        assert_eq!(error.ambiguous_transaction_id, Some(id));
    }

    #[test]
    fn snapshot_round_trips_as_versioned_json() {
        let snapshot = Snapshot::new("fixture", serde_json::json!({ "nonce": 7 }));
        let json = serde_json::to_string(&snapshot).expect("snapshot must serialize");
        let decoded: Snapshot = serde_json::from_str(&json).expect("snapshot must deserialize");

        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.version(), Snapshot::VERSION);
        assert_eq!(decoded.kind(), "fixture");
    }

    #[test]
    fn envelope_debug_redacts_signed_bytes() {
        let debug = format!("{:?}", Envelope::new([1, 2, 3]));

        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("1, 2, 3"));
    }

    #[test]
    fn prepared_round_trips_with_its_fee_and_without_exposing_its_envelope_in_debug() {
        let prepared = SignedTransaction::new(
            "fixture.signed.v1",
            Id::new("canonical-id"),
            Envelope::new([1, 2, 3]),
            "1500".parse().expect("atomic fee"),
        );
        let json = serde_json::to_vec(&prepared).expect("prepared transaction must serialize");
        let decoded: SignedTransaction =
            serde_json::from_slice(&json).expect("prepared transaction must deserialize");

        assert_eq!(decoded, prepared);
        assert_eq!(decoded.fee().to_string(), "1500");
        assert!(String::from_utf8_lossy(&json).contains("\"1500\""));
        assert!(!format!("{prepared:?}").contains("1, 2, 3"));
    }

    #[test]
    fn a_claim_is_held_in_process_and_left_out_of_the_durable_form() {
        let network = Network::new(crate::ChainName::new("test"), "mainnet");
        let claim = Claim::new([format!("{}/7", Address::new(network, "alice"))]);
        let prepared = SignedTransaction::new(
            "fixture.signed.v1",
            Id::new("canonical-id"),
            Envelope::new([1, 2, 3]),
            "1500".parse().expect("atomic fee"),
        )
        .with_claim(claim.clone());
        let json = serde_json::to_vec(&prepared).expect("prepared transaction must serialize");
        let decoded: SignedTransaction =
            serde_json::from_slice(&json).expect("prepared transaction must deserialize");

        assert_eq!(prepared.claim(), Some(&claim));
        assert_eq!(decoded.claim(), None, "a claim does not survive storage");
        assert!(!String::from_utf8_lossy(&json).contains("claim"));
        assert_eq!(decoded, prepared, "and is not part of the identity");
    }
}
