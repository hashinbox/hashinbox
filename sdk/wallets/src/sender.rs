use std::{error, fmt, future::Future, pin::Pin, sync::Arc};

use base::{Address, Decimal, TransactionId};

use crate::{Author, MAX_TRANSFERS, Wallet, WalletError, WalletErrorKind};

/// The future a [`Sender`] returns: accepted ids in order, or a [`SendError`].
pub type SendFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<TransactionId>, SendError>> + Send + 'a>>;

/// How much of a wallet's asset to send.
///
/// `All` resolves under the chain's send reservation, after earlier transfers
/// from the same source and their fees. Ethereum reserves maximum EIP-1559
/// fees, so unused gas and fee refunds may leave a residue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Amount {
    /// An exact positive amount in display units.
    Exact(Decimal),
    /// Every available atomic unit after transfers and required fees.
    All,
}

impl From<Decimal> for Amount {
    fn from(value: Decimal) -> Self {
        Self::Exact(value)
    }
}

impl std::str::FromStr for Amount {
    type Err = <Decimal as std::str::FromStr>::Err;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "all" => Ok(Self::All),
            _ => value.parse().map(Self::Exact),
        }
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(value) => value.fmt(formatter),
            Self::All => formatter.write_str("all"),
        }
    }
}

/// One requested value transfer from an already-created wallet.
pub struct Transfer {
    /// The sending wallet.
    pub wallet: Arc<dyn Wallet>,
    /// A destination in the wallet's network.
    pub to: Address,
    /// Exact display units, or all available funds after fees.
    pub amount: Amount,
}

/// A batch failure with what was already accepted, so a caller never resends it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendError {
    /// Transactions definitely acknowledged before this batch stopped.
    pub accepted: Vec<TransactionId>,
    /// Original authored item index, present only for an item-scoped failure.
    pub failed_index: Option<usize>,
    /// Canonical local ID moved from an ambiguous concrete transaction error.
    pub ambiguous_transaction_id: Option<TransactionId>,
    /// The underlying failure.
    pub source: WalletError,
}

impl SendError {
    /// Creates a definite failure that belongs to no individual item: the
    /// batch was rejected as a whole, or the operation failed before any item
    /// could be selected.
    #[must_use]
    pub fn definite(kind: WalletErrorKind, message: impl Into<String>) -> Self {
        Self {
            accepted: Vec::new(),
            failed_index: None,
            ambiguous_transaction_id: None,
            source: WalletError::new(kind, message),
        }
    }

    /// Accepts a batch of `len` transfers: at least one and at most
    /// [`MAX_TRANSFERS`].
    ///
    /// # Errors
    ///
    /// An [`WalletErrorKind::InvalidBatch`] definite failure otherwise.
    pub fn check_count(len: usize) -> Result<(), Self> {
        match len {
            0 => Err(Self::definite(
                WalletErrorKind::InvalidBatch,
                "at least one transfer is required",
            )),
            len if len > MAX_TRANSFERS => Err(Self::definite(
                WalletErrorKind::InvalidBatch,
                "at most 50 transfers are allowed",
            )),
            _ => Ok(()),
        }
    }

    /// Requires a sweep to be the final item from its source in this batch.
    ///
    /// # Errors
    /// An item-scoped invalid-batch failure before any chain call when an
    /// earlier item already swept this source, including a second sweep.
    pub fn check_selection(transfers: &[Transfer]) -> Result<(), Self> {
        for (index, item) in transfers.iter().enumerate() {
            if transfers[..index].iter().any(|earlier| {
                earlier.amount == Amount::All && earlier.wallet.address() == item.wallet.address()
            }) {
                return Err(Self::item(
                    index,
                    Vec::new(),
                    WalletError::new(
                        WalletErrorKind::InvalidBatch,
                        "a sweep must be the final transfer from its source",
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Creates a failure for one original item in an ordered batch.
    #[must_use]
    pub fn item(failed_index: usize, accepted: Vec<TransactionId>, source: WalletError) -> Self {
        Self::new(accepted, Some(failed_index), source)
    }

    /// Creates an index-free failure for one transaction grouping several items.
    #[must_use]
    pub fn grouped(accepted: Vec<TransactionId>, source: WalletError) -> Self {
        Self::new(accepted, None, source)
    }

    fn new(
        accepted: Vec<TransactionId>,
        failed_index: Option<usize>,
        mut source: WalletError,
    ) -> Self {
        let ambiguous_transaction_id = source.ambiguous_transaction_id.take();
        Self {
            accepted,
            failed_index,
            ambiguous_transaction_id,
            source,
        }
    }
}

impl fmt::Display for SendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.failed_index {
            Some(failed_index) => write!(
                formatter,
                "transaction {failed_index} failed after {} accepted transaction(s): {}",
                self.accepted.len(),
                self.source
            ),
            None => self.source.fmt(formatter),
        }
    }
}

impl error::Error for SendError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Sends a batch using the concrete chain's native transaction model.
///
/// UTXO chains may combine compatible transfers into one transaction. Account
/// chains normally submit one nonce-ordered transaction per transfer.
/// Callers route batches through [`crate::Wallets::send_all`], which proves
/// that every wallet belongs to the registered asset owning this sender.
/// Constructing [`Transfer`] values from another factory or asset is outside
/// this capability's contract.
pub trait Sender: Send + Sync {
    /// Sends the transfers in order; stops at the first failure.
    fn send<'a>(&'a self, transfers: Vec<Transfer>) -> SendFuture<'a>;
}

impl<T: Sender + ?Sized> Sender for Arc<T> {
    fn send<'a>(&'a self, transfers: Vec<Transfer>) -> SendFuture<'a> {
        (**self).send(transfers)
    }
}

/// Everything one asset's factory needs to spend it: the batch [`Sender`] its
/// wallets submit through, and the [`Author`] capability of one of its own
/// wallets.
///
/// [`crate::FactoryProfile::authorship`] returns one only for an asset this
/// system spends. An asset it receives and reports declares `None`, and that
/// single absence is what refuses every send and every draft — at
/// [`crate::Wallets`]'s lookup, before any chain code runs. A chain therefore
/// writes no gate of its own: there is no path from an unspendable asset to a
/// builder or a send pipeline to gate.
pub trait Authorship: Sender {
    /// `wallet`, one this asset's own factory made, with its authoring
    /// capability.
    ///
    /// Membership is [`crate::WalletFactory::validate`]'s answer and
    /// [`crate::Wallets::author`] has already taken it; this only recovers the
    /// concrete type the capability lives on.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unsupported`] when `wallet` is not one of this asset's.
    fn author(&self, wallet: &Arc<dyn Wallet>) -> Result<Arc<dyn Author>, WalletError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definite_failure_has_no_transaction_metadata_or_indexed_display() {
        for (kind, message) in [
            (
                WalletErrorKind::InvalidBatch,
                "at least one transfer is required",
            ),
            (WalletErrorKind::Unavailable, "account acquisition failed"),
        ] {
            let failure = SendError::definite(kind, message);

            assert!(failure.accepted.is_empty());
            assert_eq!(failure.failed_index, None);
            assert_eq!(failure.ambiguous_transaction_id, None);
            assert_eq!(failure.source.kind, kind);
            assert_eq!(failure.to_string(), message);
            assert!(!failure.to_string().contains("transaction 0"));
        }
    }

    #[test]
    fn count_check_accepts_one_to_fifty_transfers() {
        for len in [1, MAX_TRANSFERS] {
            assert_eq!(SendError::check_count(len), Ok(()));
        }
        for (len, message) in [
            (0, "at least one transfer is required"),
            (MAX_TRANSFERS + 1, "at most 50 transfers are allowed"),
        ] {
            let failure = SendError::check_count(len).unwrap_err();
            assert_eq!(failure.source.kind, WalletErrorKind::InvalidBatch);
            assert_eq!(failure.to_string(), message);
            assert_eq!(failure.failed_index, None);
        }
    }

    #[test]
    fn item_failure_preserves_original_index_and_acknowledged_prefix() {
        let accepted = vec![TransactionId::new("first"), TransactionId::new("second")];
        let ambiguous = TransactionId::new("item-canonical-id");
        let failure = SendError::item(
            4,
            accepted.clone(),
            WalletError::from(
                base::TransactionError::new(
                    base::TransactionErrorKind::Timeout,
                    "item submission outcome is unknown",
                )
                .with_ambiguous_transaction_id(ambiguous.clone()),
            ),
        );

        assert_eq!(failure.accepted, accepted);
        assert_eq!(failure.failed_index, Some(4));
        assert_eq!(failure.ambiguous_transaction_id, Some(ambiguous));
        assert_eq!(failure.source.ambiguous_transaction_id, None);
        assert_eq!(
            failure.to_string(),
            "transaction 4 failed after 2 accepted transaction(s): item submission \
            outcome is unknown"
        );
    }

    #[test]
    fn grouped_failure_moves_ambiguity_without_inventing_an_item_index() {
        let ambiguous = TransactionId::new("grouped-canonical-id");
        let source = WalletError::from(
            base::TransactionError::new(
                base::TransactionErrorKind::Timeout,
                "grouped submission outcome is unknown",
            )
            .with_ambiguous_transaction_id(ambiguous.clone()),
        );
        let accepted = vec![TransactionId::new("acknowledged-prefix")];

        let failure = SendError::grouped(accepted.clone(), source);

        assert_eq!(failure.accepted, accepted);
        assert_eq!(failure.failed_index, None);
        assert_eq!(failure.ambiguous_transaction_id, Some(ambiguous));
        assert_eq!(failure.source.ambiguous_transaction_id, None);
        assert_eq!(failure.to_string(), "grouped submission outcome is unknown");
    }
}
