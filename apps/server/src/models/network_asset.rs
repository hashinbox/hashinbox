//! Every asset of a network as stored: the `network_asset` table.

use std::collections::BTreeMap;

use serde::Serialize;

/// What a network asset is.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NetworkAssetKind {
    /// The network's own currency.
    Native,
    /// A token, by its contract (mint).
    Contract,
}

/// One asset of a network: its native currency or a token.
#[derive(Clone, Debug, Serialize, PartialEq, Eq, utoipa::ToSchema)]
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

/// The `network_asset` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;
    use anyhow::Context as _;
    use diesel::{prelude::*, sqlite::Sqlite};

    use crate::models::schema::network_asset;

    /// One `network_asset` row as the file holds it.
    #[derive(Queryable, Selectable)]
    #[diesel(table_name = network_asset, check_for_backend(Sqlite))]
    struct Row {
        chain: String,
        network: String,
        asset: String,
        kind: String,
        contract: Option<String>,
        ticker: String,
        decimals: i64,
        display_decimals: Option<i64>,
    }

    impl TryFrom<Row> for (base::Network, NetworkAsset) {
        type Error = anyhow::Error;

        fn try_from(row: Row) -> anyhow::Result<Self> {
            let digits = |value: i64| u32::try_from(value).context("asset decimals out of range");
            let kind = match row.kind.as_str() {
                "native" => NetworkAssetKind::Native,
                "contract" => NetworkAssetKind::Contract,
                other => anyhow::bail!("{}/{}: asset kind {other:?}", row.chain, row.network),
            };
            let asset = NetworkAsset {
                asset: row.asset,
                kind,
                contract: row.contract,
                ticker: row.ticker,
                decimals: digits(row.decimals)?,
                display_decimals: row.display_decimals.map(digits).transpose()?,
            };
            Ok((
                base::Network::new(base::Chain::new(row.chain), row.network),
                asset,
            ))
        }
    }

    /// Every network's assets, each network's in `asset` order.
    ///
    /// # Errors
    ///
    /// The read, or a row out of range.
    pub(crate) fn find_all(
        connection: &mut SqliteConnection,
    ) -> anyhow::Result<BTreeMap<base::Network, Vec<NetworkAsset>>> {
        let rows = network_asset::table
            .select(Row::as_select())
            .order((
                network_asset::chain,
                network_asset::network,
                network_asset::asset,
            ))
            .load(connection)?;
        let mut assets = BTreeMap::<_, Vec<_>>::new();
        for row in rows {
            let (network, asset) = row.try_into()?;
            assets.entry(network).or_default().push(asset);
        }
        Ok(assets)
    }
}
