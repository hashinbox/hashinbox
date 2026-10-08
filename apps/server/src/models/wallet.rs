//! A wallet as the routes publish it: one row of the SDK's `wallet` table —
//! one address of one network — read through the repository's
//! `wallets::WalletRepository` (the payments container's slot), with the assets the index observes
//! of it under
//! their configured tags. The server keeps no wallet table of its own; the key
//! stays in the row and reaches this process only as signers
//! (`models::Signers`).

use std::{fmt, sync::Arc};

use serde::{Deserialize, Serialize};
use wallets::{WalletError, WalletErrorKind};

use indexing::Watching as _;
use payments::Payments;

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

/// One asset the index observes a wallet's address in.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct WatchedAsset {
    /// The configured asset tag; null when no configured asset is it any
    /// more. Always present.
    #[schema(required, value_type = Option<String>, pattern = "^[a-z0-9][a-z0-9-]{0,31}$")]
    asset: Option<AssetTag>,
    /// The asset's identity within its chain: `native`, or a token
    /// identifier.
    asset_id: String,
}

/// One wallet: one address of one network, its lifecycle, and the assets it
/// is observed in. Its wire form is what every wallet route returns; it never
/// holds the key.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct Wallet {
    /// Wallet ID, a UUIDv7 the API minted.
    id: String,
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
    /// The assets the index observes the address in, in asset order; empty
    /// once archived or when this process does not serve its network.
    assets: Vec<WatchedAsset>,
    #[serde(skip)]
    address_value: base::Address,
}

impl Wallet {
    /// `stored` as published, with `watched`, the index's watches of it.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unavailable`] for a stored label this build does
    /// not accept; the label is not quoted.
    fn from_stored(
        stored: &wallets::StoredWallet,
        watched: &[indexing::Watch],
        wallets: &Wallets,
    ) -> Result<Self, WalletError> {
        let address = stored.address().clone();
        let configured = wallets.assets();
        let assets = watched
            .iter()
            .filter(|watch| watch.id == stored.id())
            .map(|watch| WatchedAsset {
                asset: configured
                    .iter()
                    .find(|info| info.network == address.network && info.id == watch.asset)
                    .map(|info| info.tag.clone()),
                asset_id: watch.asset.asset.clone(),
            })
            .collect();
        let label = stored
            .label()
            .map(|text| Label::try_from(text.to_owned()))
            .transpose()
            .map_err(|_| Self::failure("a stored wallet label is not a valid label"))?;
        Ok(Self {
            id: stored.id().to_owned(),
            network: address.network.clone().into(),
            address: address.value.clone(),
            watch_only: stored.key().is_none(),
            label,
            archived_at: stored.archived_at(),
            assets,
            address_value: address,
        })
    }

    /// Every watch the index holds on `network`; none when this process does
    /// not serve it.
    async fn watched(index: &indexing::Indexers, network: &base::Network) -> Vec<indexing::Watch> {
        index.watched(network).await.unwrap_or_default()
    }

    /// The wallet `id`, archived or not.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::NotFound`] when there is none, and
    /// [`WalletErrorKind::Unavailable`] when the read failed.
    pub(crate) async fn find(payments: &Payments<AssetTag>, id: &str) -> Result<Self, WalletError> {
        let (index, wallets) = (&payments.index, &payments.wallets);
        let stored = Self::registry(payments)?
            .wallet(id)
            .await?
            .ok_or_else(Self::unknown)?;
        let watched = Self::watched(index, &stored.address().network).await;
        Self::from_stored(&stored, &watched, wallets)
    }

    /// Every wallet, archived ones included, in id order.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unavailable`] when the read failed.
    pub(crate) async fn all(payments: &Payments<AssetTag>) -> Result<Vec<Self>, WalletError> {
        let (index, wallets) = (&payments.index, &payments.wallets);
        let stored = Self::registry(payments)?.wallets().await?;
        let mut watched = Vec::new();
        for network in index.networks() {
            watched.extend(Self::watched(index, network).await);
        }
        stored
            .iter()
            .map(|wallet| Self::from_stored(wallet, &watched, wallets))
            .collect()
    }

    pub(crate) const fn archived(&self) -> bool {
        self.archived_at.is_some()
    }

    /// The wallet's address.
    pub(crate) const fn address(&self) -> &base::Address {
        &self.address_value
    }

    /// What the index reads this wallet as in configured asset `tag`.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::NotFound`] when `tag` is not an asset of this
    /// wallet's network this process serves.
    pub(crate) fn subject(
        &self,
        wallets: &Wallets,
        tag: &AssetTag,
    ) -> Result<indexing::Subject, WalletError> {
        let info = wallets
            .asset(tag)
            .ok()
            .filter(|info| info.network == self.address_value.network)
            .ok_or_else(|| {
                WalletError::new(
                    WalletErrorKind::NotFound,
                    "the asset is not one of this wallet's network",
                )
            })?;
        Ok(indexing::Subject {
            address: self.address_value.clone(),
            asset: info.id,
        })
    }

    /// The wallet that signs this one's sends of asset `tag`.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Conflict`] once archived, [`WalletErrorKind::WatchOnly`]
    /// without a key, and [`WalletErrorKind::Unavailable`] when its key was
    /// not loaded for that asset because its chain is not in service.
    pub(crate) fn signer(
        &self,
        signers: &Signers,
        tag: &AssetTag,
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
            .get(&self.id, tag)?
            .ok_or_else(|| Self::failure("the wallet's chain is not in service"))
    }

    fn unknown() -> WalletError {
        WalletError::new(WalletErrorKind::NotFound, "wallet does not exist")
    }

    fn failure(message: &'static str) -> WalletError {
        WalletError::new(WalletErrorKind::Unavailable, message)
    }

    /// The wallet rows the payments container was given.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unavailable`] when no wallet repository is attached.
    pub(crate) fn registry(
        payments: &Payments<AssetTag>,
    ) -> Result<&dyn wallets::WalletRepository, WalletError> {
        payments
            .registry
            .as_deref()
            .ok_or_else(|| Self::failure("no wallet repository is attached"))
    }
}

impl fmt::Debug for Wallet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Wallet")
            .field("id", &self.id)
            .field("address", &self.address)
            .field("watch_only", &self.watch_only)
            .field("archived_at", &self.archived_at)
            .finish_non_exhaustive()
    }
}
