use std::{error, fmt, str::FromStr};

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Payments, Wallets, contract::Network, error::ErrorBody};

/// Metadata of the asset an amount is denominated in.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub(super) struct Asset {
    /// Chain name.
    pub(super) chain: String,
    /// Asset ID within the chain (`native` or a token identifier).
    pub(super) id: String,
    /// Display name, when known; always present.
    #[schema(required)]
    pub(super) name: Option<String>,
    /// Ticker symbol, when known; always present.
    #[schema(required)]
    pub(super) ticker: Option<String>,
    /// Decimal places between atomic and display units.
    pub(super) decimals: u32,
}

impl Asset {
    /// The asset `id` names, described by the unit its own chain declared.
    pub(super) fn new(id: &base::AssetId, unit: &base::Unit) -> Self {
        Self {
            chain: id.chain.to_string(),
            id: id.asset.clone(),
            name: unit.name.clone(),
            ticker: unit.ticker.clone(),
            decimals: unit.decimals,
        }
    }
}

pub(super) fn routes() -> OpenApiRouter<Payments> {
    OpenApiRouter::new().routes(routes!(list))
}

/// One configured asset: its tag, where its wallets live, and what they hold.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct Listing {
    /// The tag every wallet route takes (`GET /v1/assets` is the whole list).
    #[schema(pattern = "^[a-z0-9][a-z0-9-]{0,31}$")]
    asset: String,
    /// The chain and network this asset's wallets live on.
    network: Network,
    /// The held asset's metadata.
    metadata: Asset,
}

impl Listing {
    /// One registered asset — what it is — described by `unit`, the unit its
    /// own chain declared for it.
    fn new(info: wallets::AssetInfo<AssetTag>, unit: &base::Unit) -> Self {
        Self {
            asset: info.tag.to_string(),
            network: info.network.into(),
            metadata: Asset::new(&info.id, unit),
        }
    }
}

/// `GET /v1/assets`: every configured asset.
// linter:disable rust/redundant-module-prefix -- Utoipa publishes the schema under this name.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct AssetList {
    /// Assets in tag order.
    assets: Vec<Listing>,
}

impl AssetList {
    /// Every registered asset of `wallets` — what it is — described by the
    /// unit its own chain declared in `units`.
    ///
    /// The two halves of one `wallets::Composition` meet here and nowhere
    /// earlier. Composition refuses to start a process whose factories leave
    /// an asset's unit undeclared, so every registered tag is listed.
    fn new(wallets: &Wallets, units: &base::Units) -> Self {
        let assets = wallets
            .assets()
            .into_iter()
            .filter_map(|info| {
                let unit = units.of(&info.network, &info.id)?;
                Some(Listing::new(info, unit))
            })
            .collect();
        Self { assets }
    }
}

#[utoipa::path(
    get,
    path = "/v1/assets",
    operation_id = "list_assets",
    summary = "List every configured asset",
    description = "Every asset tag registered at startup, with its network and metadata. This \
        is the only list of tags the wallet routes accept.",
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Configured assets in tag order", body = AssetList),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody)
    ),
    tag = "assets"
)]
async fn list(State(payments): State<Payments>) -> Json<AssetList> {
    Json(AssetList::new(&payments.wallets, &payments.units))
}

/// The user-facing key of one registered asset: `btc`, `eth`, `usdc`,
/// `eth-sepolia`.
///
/// One tag names one wallet factory (one asset in one network) at the
/// composition root; everything after composition selects the asset by its
/// tag, never by a chain enum. Text is `[a-z0-9][a-z0-9-]{0,31}`.
// linter:disable rust/redundant-module-prefix -- The crate names it `api::AssetTag`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct AssetTag(String);

impl AssetTag {
    /// Longest accepted tag, in bytes.
    const MAX_LEN: usize = 32;

    /// Validates owned tag text.
    ///
    /// # Errors
    ///
    /// [`TagError`] when `tag` is not `[a-z0-9][a-z0-9-]{0,31}`.
    fn new(tag: impl Into<String>) -> Result<Self, TagError> {
        let tag = tag.into();
        if Self::accepts(tag.as_bytes()) {
            Ok(Self(tag))
        } else {
            Err(TagError)
        }
    }

    const fn accepts(bytes: &[u8]) -> bool {
        if bytes.is_empty() || bytes.len() > Self::MAX_LEN || bytes[0] == b'-' {
            return false;
        }
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            if !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-') {
                return false;
            }
            index += 1;
        }
        true
    }
}

impl fmt::Display for AssetTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for AssetTag {
    type Err = TagError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::new(text)
    }
}

impl TryFrom<String> for AssetTag {
    type Error = TagError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::new(text)
    }
}

impl From<AssetTag> for String {
    fn from(tag: AssetTag) -> Self {
        tag.0
    }
}

/// Text that is not an [`AssetTag`].
// linter:disable rust/empty-struct -- The failure has one cause; its message is the whole value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TagError;

impl fmt::Display for TagError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("asset tag must match [a-z0-9][a-z0-9-]{0,31}")
    }
}

impl error::Error for TagError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_tag_accepts_only_lowercase_slugs() {
        for accepted in ["btc", "eth", "eth-sepolia", "0x", "a", &"a".repeat(32)] {
            let tag: AssetTag = accepted.parse().expect("valid tag");
            assert_eq!(tag.to_string(), accepted);
        }
        for rejected in [
            "",
            "-eth",
            "ETH",
            "eth_x",
            "eth sepolia",
            "é",
            &"a".repeat(33),
        ] {
            assert_eq!(rejected.parse::<AssetTag>(), Err(TagError), "{rejected}");
        }
        assert_eq!(
            serde_json::from_str::<AssetTag>(r#""usdc""#).expect("tag JSON"),
            "usdc".parse::<AssetTag>().expect("valid tag")
        );
        assert!(serde_json::from_str::<AssetTag>(r#""USDC""#).is_err());
        assert_eq!(
            serde_json::to_string(&"sol".parse::<AssetTag>().expect("valid tag"))
                .expect("tag JSON"),
            r#""sol""#
        );
    }
}
