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

/// The asset as its own chain declared it.
impl From<&base::AssetInfo> for Asset {
    fn from(info: &base::AssetInfo) -> Self {
        Self {
            chain: info.asset.network.chain.to_string(),
            id: info.asset.id.clone(),
            name: info.name.clone(),
            ticker: info.ticker.clone(),
            decimals: info.decimals,
        }
    }
}
