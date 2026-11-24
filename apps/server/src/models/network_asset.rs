//! Every asset of a network as stored: the `network_asset` table, read with its
//! network (`network::db`).

use serde::{Deserialize, Serialize};

/// What a network asset is.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NetworkAssetKind {
    /// The network's own currency.
    Native,
    /// A token, by its contract (mint).
    Contract,
}

/// One asset of a network: its native currency or a token.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub(crate) struct NetworkAsset {
    /// `native`, or the token's contract (mint).
    pub(crate) asset: String,
    pub(crate) kind: NetworkAssetKind,
    /// The token's contract (mint); always present, null for the native
    /// currency.
    #[schema(required)]
    pub(crate) contract: Option<String>,
    /// Display symbol.
    pub(crate) ticker: String,
    /// Fractional digits between the display unit and the atomic unit.
    pub(crate) decimals: u32,
    /// Fractional digits a view shows; always present, null: all.
    #[schema(required)]
    pub(crate) display_decimals: Option<u32>,
}

impl From<&NetworkAsset> for base::AssetConfig {
    fn from(asset: &NetworkAsset) -> Self {
        Self {
            asset: asset.asset.clone(),
            kind: match asset.kind {
                NetworkAssetKind::Native => base::AssetKind::Native,
                NetworkAssetKind::Contract => base::AssetKind::Contract,
            },
            contract: asset.contract.clone(),
            ticker: asset.ticker.clone(),
            decimals: asset.decimals,
            display_decimals: asset.display_decimals,
        }
    }
}
