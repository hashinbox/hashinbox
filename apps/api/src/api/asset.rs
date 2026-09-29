use std::{borrow::Cow, error, fmt, str::FromStr};

use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{PathRejection, QueryRejection},
    },
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{
    Payments, Wallets,
    contract::{Address, Network},
    error::{Error, ErrorBody},
};

/// Metadata of the asset an amount is denominated in.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Asset {
    /// Chain name.
    pub chain: String,
    /// Asset ID within the chain (`native` or a token identifier).
    pub id: String,
    /// Display name, when known; always present.
    #[schema(required)]
    pub name: Option<String>,
    /// Ticker symbol, when known; always present.
    #[schema(required)]
    pub ticker: Option<String>,
    /// Decimal places between atomic and display units.
    pub decimals: u32,
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

pub fn routes() -> OpenApiRouter<Payments> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(address))
}

/// One configured asset: its tag, where its wallets live, and what they hold.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Listing {
    /// The tag every wallet route takes (`GET /v1/assets` is the whole list).
    #[schema(pattern = "^[a-z0-9][a-z0-9-]{0,31}$")]
    pub asset: String,
    /// The chain and network this asset's wallets live on.
    pub network: Network,
    /// The held asset's metadata.
    pub metadata: Asset,
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
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct AssetList {
    /// Assets in tag order.
    pub assets: Vec<Listing>,
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
                let unit = units.of(&info.id)?;
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

/// Path of `GET /v1/assets/{asset}/address`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Path)]
pub struct AssetPath {
    /// A configured asset tag.
    #[param(value_type = String, pattern = "^[a-z0-9][a-z0-9-]{0,31}$")]
    pub asset: AssetTag,
}

/// Query of `GET /v1/assets/{asset}/address`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct AddressQuery {
    /// Address text in any form the asset's chain accepts. It is arbitrary
    /// caller text, so it travels in the query string, never in a path segment.
    pub text: String,
}

#[utoipa::path(
    get,
    path = "/v1/assets/{asset}/address",
    operation_id = "read_asset_address",
    summary = "Parse address text into its canonical form",
    description = "The canonical address of `text` in the asset's network, parsed by the \
        asset's chain without a wallet: validates a destination or an address to watch.",
    params(AssetPath, AddressQuery),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The canonical address", body = Address),
        (status = 400, description = "Malformed asset tag or address text", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Asset is not configured", body = ErrorBody)
    ),
    tag = "assets"
)]
async fn address(
    State(payments): State<Payments>,
    path: Result<Path<AssetPath>, PathRejection>,
    query: Result<Query<AddressQuery>, QueryRejection>,
) -> Result<Json<Address>, Error> {
    let Path(path) = path.map_err(Error::invalid_path)?;
    let Query(query) = query.map_err(Error::invalid_query)?;
    let address = payments
        .wallets
        .parse(&path.asset, &query.text)
        .map_err(Error::by_asset)?;
    Ok(Json(address.into()))
}

/// The user-facing key of one registered asset: `btc`, `eth`, `usdc`,
/// `eth-sepolia`.
///
/// One tag names one wallet factory (one asset in one network) at the
/// composition root; everything after composition selects the asset by its
/// tag, never by a chain enum. Text is `[a-z0-9][a-z0-9-]{0,31}`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AssetTag(Cow<'static, str>);

impl AssetTag {
    /// Longest accepted tag, in bytes.
    pub const MAX_LEN: usize = 32;

    /// Declares a tag as a `const`; an invalid literal fails to compile in a
    /// `const` context and panics otherwise.
    #[must_use]
    pub const fn from_static(tag: &'static str) -> Self {
        assert!(Self::accepts(tag.as_bytes()), "invalid asset tag");
        Self(Cow::Borrowed(tag))
    }

    /// Validates owned tag text.
    ///
    /// # Errors
    ///
    /// [`TagError`] when `tag` is not `[a-z0-9][a-z0-9-]{0,31}`.
    pub fn new(tag: impl Into<String>) -> Result<Self, TagError> {
        let tag = tag.into();
        if Self::accepts(tag.as_bytes()) {
            Ok(Self(Cow::Owned(tag)))
        } else {
            Err(TagError)
        }
    }

    /// The exact tag text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
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
        tag.0.into_owned()
    }
}

/// Text that is not an [`AssetTag`].
// linter:disable rust/empty-struct -- The failure has one cause; its message is the whole value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TagError;

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
        const BTC: AssetTag = AssetTag::from_static("btc");
        assert_eq!(BTC.as_str(), "btc");
        for accepted in ["eth", "eth-sepolia", "0x", "a", &"a".repeat(32)] {
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
            AssetTag::from_static("usdc")
        );
        assert!(serde_json::from_str::<AssetTag>(r#""USDC""#).is_err());
        assert_eq!(
            serde_json::to_string(&AssetTag::from_static("sol")).expect("tag JSON"),
            r#""sol""#
        );
    }
}
