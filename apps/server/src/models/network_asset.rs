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

/// The `network_asset` table's own statements (its rows are read with their
/// network, `network::db`).
pub(crate) mod db {
    use diesel::{prelude::*, sqlite::Sqlite};

    use super::{NetworkAsset, NetworkAssetKind};
    use crate::models::schema::network_asset;

    /// One `network_asset` row but its network.
    #[derive(Queryable, Selectable)]
    #[diesel(table_name = network_asset, check_for_backend(Sqlite))]
    struct Row {
        asset: String,
        kind: String,
        contract: Option<String>,
        ticker: String,
        decimals: i64,
        display_decimals: Option<i64>,
    }

    /// One asset of each ticker (the first by network), sorted by ticker:
    /// what the market job prices.
    ///
    /// # Errors
    ///
    /// When the read fails, or a row does not decode.
    pub(crate) fn find_one_per_ticker(
        connection: &mut SqliteConnection,
    ) -> anyhow::Result<Vec<NetworkAsset>> {
        use network_asset::dsl;
        let rows: Vec<Row> = dsl::network_asset
            .select(Row::as_select())
            .order((dsl::ticker, dsl::chain, dsl::network, dsl::asset))
            .load(connection)?;
        let mut assets = Vec::with_capacity(rows.len());
        for row in rows {
            if assets
                .last()
                .is_some_and(|last: &NetworkAsset| last.ticker == row.ticker)
            {
                continue;
            }
            assets.push(decoded(row)?);
        }
        Ok(assets)
    }

    /// Every asset of network `chain`/`network`.
    ///
    /// # Errors
    ///
    /// When the read fails, or a row does not decode.
    pub(crate) fn find_of_network(
        connection: &mut SqliteConnection,
        chain: &str,
        network: &str,
    ) -> anyhow::Result<Vec<NetworkAsset>> {
        use network_asset::dsl;
        let rows: Vec<Row> = dsl::network_asset
            .filter(dsl::chain.eq(chain))
            .filter(dsl::network.eq(network))
            .select(Row::as_select())
            .load(connection)?;
        rows.into_iter().map(decoded).collect()
    }

    /// Keeps `asset` of network `chain`/`network`.
    ///
    /// # Errors
    ///
    /// The write.
    pub(crate) fn insert(
        connection: &mut SqliteConnection,
        chain: &str,
        network: &str,
        asset: &NetworkAsset,
    ) -> anyhow::Result<()> {
        let kind = match asset.kind {
            NetworkAssetKind::Native => "native",
            NetworkAssetKind::Contract => "contract",
        };
        diesel::insert_into(network_asset::table)
            .values((
                network_asset::chain.eq(chain),
                network_asset::network.eq(network),
                network_asset::asset.eq(&asset.asset),
                network_asset::kind.eq(kind),
                network_asset::contract.eq(&asset.contract),
                network_asset::ticker.eq(&asset.ticker),
                network_asset::decimals.eq(i64::from(asset.decimals)),
                network_asset::display_decimals.eq(asset.display_decimals.map(i64::from)),
            ))
            .execute(connection)?;
        Ok(())
    }

    /// `row` decoded.
    fn decoded(row: Row) -> anyhow::Result<NetworkAsset> {
        Ok(NetworkAsset {
            asset: row.asset,
            kind: serde_json::from_value::<NetworkAssetKind>(row.kind.into())?,
            contract: row.contract,
            ticker: row.ticker,
            decimals: u32::try_from(row.decimals)?,
            display_decimals: row.display_decimals.map(u32::try_from).transpose()?,
        })
    }
}
