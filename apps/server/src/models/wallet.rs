//! A wallet as the routes publish it: one row of indexing's `wallet` table,
//! read through the repository's `indexing::WalletRegistry`, under the
//! configured asset tag that holds it. The server keeps no wallet table of its
//! own; the key stays in the row and reaches this process only as a signer
//! (`models::Signers`).

use std::{fmt, sync::Arc};

use serde::{Deserialize, Serialize};
use wallets::{WalletError, WalletErrorKind};

use super::{Network, Signers};
use crate::asset::{AssetTag, Wallets};

/// An operator's name for a wallet: 1 to 64 characters, no control
/// characters.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(try_from = "String", into = "String")]
#[schema(value_type = String)]
pub(crate) struct Label(String);

impl TryFrom<String> for Label {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        let length = text.chars().count();
        if !(1..=64).contains(&length) || text.chars().any(char::is_control) {
            return Err("a wallet label is 1 to 64 characters, none of them control".into());
        }
        Ok(Self(text))
    }
}

impl From<Label> for String {
    fn from(label: Label) -> Self {
        label.0
    }
}

/// One wallet: its public identity and its lifecycle. Its wire form is what
/// every wallet route returns; it never holds the key.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct Wallet {
    /// Wallet ID, a UUIDv7 the API minted.
    id: String,
    /// The configured asset tag this wallet holds and sends; null when no
    /// configured asset holds it any more. Always present.
    #[schema(required, value_type = Option<String>, pattern = "^[a-z0-9][a-z0-9-]{0,31}$")]
    asset: Option<AssetTag>,
    /// The asset's identity within its chain: `native`, or a token
    /// identifier.
    asset_id: String,
    /// The chain and network of the wallet's address.
    network: Network,
    /// Canonical address text.
    address: String,
    /// Observed without a key: history and balance only, no sends.
    watch_only: bool,
    /// The operator's label; null when none was given. Always present.
    #[schema(required, value_type = Option<String>)]
    label: Option<Label>,
    /// Unix seconds when the wallet was archived; null while it is active.
    /// Always present.
    #[schema(required)]
    archived_at: Option<u64>,
    #[serde(skip)]
    subject: indexing::Subject,
}

impl Wallet {
    /// `stored` as published, under the configured tag of its asset.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unavailable`] for a stored label this build does
    /// not accept; the label is not quoted.
    pub(crate) fn from_stored(
        stored: &indexing::Wallet,
        wallets: &Wallets,
    ) -> Result<Self, WalletError> {
        use indexing::Holder as _;
        let subject = indexing::Subject {
            address: stored.address().clone(),
            asset: stored.asset().clone(),
        };
        let asset = wallets
            .assets()
            .into_iter()
            .find(|info| info.network == subject.address.network && info.id == subject.asset)
            .map(|info| info.tag);
        let label = stored
            .label()
            .map(|text| Label::try_from(text.to_owned()))
            .transpose()
            .map_err(|_| Self::failure("a stored wallet label is not a valid label"))?;
        Ok(Self {
            id: stored.id().to_owned(),
            asset,
            asset_id: subject.asset.asset.clone(),
            network: subject.address.network.clone().into(),
            address: subject.address.value.clone(),
            watch_only: stored.secret().is_none(),
            label,
            archived_at: stored.archived_at(),
            subject,
        })
    }

    /// The wallet `id`, archived or not.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::NotFound`] when there is none, and
    /// [`WalletErrorKind::Unavailable`] when the read failed.
    pub(crate) async fn find(
        registry: &dyn indexing::WalletRegistry,
        wallets: &Wallets,
        id: &str,
    ) -> Result<Self, WalletError> {
        let stored = registry
            .wallet(id)
            .await
            .map_err(Self::unavailable)?
            .ok_or_else(Self::unknown)?;
        Self::from_stored(&stored, wallets)
    }

    /// Every wallet, archived ones included, in id order.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unavailable`] when the read failed.
    pub(crate) async fn all(
        registry: &dyn indexing::WalletRegistry,
        wallets: &Wallets,
    ) -> Result<Vec<Self>, WalletError> {
        let stored = registry.wallets().await.map_err(Self::unavailable)?;
        stored
            .iter()
            .map(|wallet| Self::from_stored(wallet, wallets))
            .collect()
    }

    pub(crate) const fn archived(&self) -> bool {
        self.archived_at.is_some()
    }

    /// The configured tag of this wallet's asset.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::NotFound`] when this process serves no asset of
    /// it: its chain was not composed, or its asset is no longer configured.
    pub(crate) fn asset(&self) -> Result<&AssetTag, WalletError> {
        self.asset.as_ref().ok_or_else(Self::unknown)
    }

    /// What the index observes this wallet as.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::NotFound`] when this process serves no asset of it,
    /// as [`Self::asset`].
    pub(crate) fn subject(&self) -> Result<&indexing::Subject, WalletError> {
        self.asset()?;
        Ok(&self.subject)
    }

    /// The wallet that signs this one's sends.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Conflict`] once archived, [`WalletErrorKind::WatchOnly`]
    /// without a key, and [`WalletErrorKind::Unavailable`] when its key was
    /// not loaded because its chain is not in service.
    pub(crate) fn signer(
        &self,
        signers: &Signers,
    ) -> Result<Arc<dyn wallets::Wallet>, WalletError> {
        if self.archived() {
            return Err(WalletError::new(
                WalletErrorKind::Conflict,
                "the wallet is archived and sends nothing",
            ));
        }
        if self.watch_only {
            return Err(WalletError::new(
                WalletErrorKind::WatchOnly,
                "a watch-only address has no key to read a balance or send",
            ));
        }
        signers
            .get(&self.id)?
            .ok_or_else(|| Self::failure("the wallet's chain is not in service"))
    }

    fn unknown() -> WalletError {
        WalletError::new(WalletErrorKind::NotFound, "wallet does not exist")
    }

    fn failure(message: &'static str) -> WalletError {
        WalletError::new(WalletErrorKind::Unavailable, message)
    }

    /// A registry failure as the routes answer it; its messages never carry
    /// a stored value.
    fn unavailable(error: indexing::IndexError) -> WalletError {
        WalletError::new(
            WalletErrorKind::Unavailable,
            format!("wallet registry: {}", error.message),
        )
    }
}

impl fmt::Debug for Wallet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Wallet")
            .field("id", &self.id)
            .field("asset", &self.asset)
            .field("address", &self.address)
            .field("watch_only", &self.watch_only)
            .field("archived_at", &self.archived_at)
            .finish_non_exhaustive()
    }
}
