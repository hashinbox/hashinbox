//! Metadata of the asset an amount is denominated in.

use serde::{Deserialize, Serialize};

/// Metadata of the asset an amount is denominated in.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub(crate) struct Asset {
    /// Chain name.
    pub(crate) chain: String,
    /// Asset ID within the chain (`native` or a token identifier).
    pub(crate) id: String,
    /// Display name, when known; always present.
    #[schema(required)]
    pub(crate) name: Option<String>,
    /// Ticker symbol, when known; always present.
    #[schema(required)]
    pub(crate) ticker: Option<String>,
    /// Decimal places between atomic and display units.
    pub(crate) decimals: u32,
}

impl Asset {
    /// The asset `id` names, described by the unit its own chain declared.
    pub(crate) fn new(id: &base::AssetId, unit: &base::Unit) -> Self {
        Self {
            chain: id.chain.to_string(),
            id: id.asset.clone(),
            name: unit.name.clone(),
            ticker: unit.ticker.clone(),
            decimals: unit.decimals,
        }
    }
}
