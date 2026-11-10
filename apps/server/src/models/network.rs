//! The chain and network something belongs to, as one nested object; and
//! each network as stored: its `network` row with its assets, endpoints and
//! index.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{NetworkAsset, NetworkIndex, RpcNode};

/// The chain and network a wallet, address, or transaction belongs to. It
/// always ships as one nested object, never as two sibling fields.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = Network)]
pub(crate) struct NetworkName {
    /// Chain name.
    pub(crate) chain: String,
    /// Configured network name, e.g. `regtest`.
    pub(crate) name: String,
}

impl From<base::Network> for NetworkName {
    fn from(value: base::Network) -> Self {
        Self {
            chain: value.chain.to_string(),
            name: value.name,
        }
    }
}

/// One network as stored: its `network` row, its `network_asset` rows, its
/// `network_endpoint` nodes and its `network_index` row (`None`: not
/// indexed); and the node's tip asked now (`tip_live`, the stored `tip`
/// until a caller that asked sets it; `tip_error`, why asking failed).
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = ConfiguredNetwork)]
pub(crate) struct Network {
    /// Chain name.
    pub(crate) chain: String,
    /// Network name.
    pub(crate) network: String,
    /// The chain's own id of the network (Ethereum chain id, Solana genesis
    /// hash); always present, null where the name is enough.
    #[schema(required)]
    pub(crate) chain_id: Option<String>,
    /// Whether its coins are test coins.
    pub(crate) testnet: bool,
    /// Whether the server starts indexing it.
    pub(crate) enabled: bool,
    /// The chain's block (slot) time, in milliseconds.
    pub(crate) block_time_ms: u64,
    /// Positions below the tip before a block counts as confirmed.
    pub(crate) confirmations: u64,
    /// How old, in days, a wallet's start may be; always present, null: no
    /// limit.
    #[schema(required)]
    pub(crate) max_history_days: Option<u64>,
    /// An address's explorer page, `{address}` filled in; always present,
    /// null: none.
    #[schema(required)]
    pub(crate) explorer_address: Option<String>,
    /// A transaction's explorer page, `{tx}` filled in; always present,
    /// null: none.
    #[schema(required)]
    pub(crate) explorer_transaction: Option<String>,
    /// The chain's own knobs; an absent key is the chain's default.
    #[schema(value_type = Object)]
    pub(crate) settings: Value,
    /// Its native currency and tokens.
    pub(crate) assets: Vec<NetworkAsset>,
    /// Its nodes in failover order.
    pub(crate) endpoints: Vec<RpcNode>,
    /// Its indexing; always present, null: not indexed.
    #[schema(required)]
    pub(crate) index: Option<NetworkIndex>,
    /// Why its index job's last run failed (the job's `reason`), cleared by
    /// the next run that does not; always present, null: none.
    #[schema(required)]
    pub(crate) index_job_reason: Option<String>,
    /// The node's newest block, asked now; the stored `tip` when that
    /// failed. Always present.
    #[schema(required)]
    pub(crate) tip_live: Option<u64>,
    /// Why asking the node failed; always present, null when it answered.
    #[schema(required)]
    pub(crate) tip_error: Option<String>,
    /// The nodes, for the chain.
    #[serde(skip)]
    pub(crate) nodes: Vec<rpc::Endpoint>,
}

impl Network {
    /// Its chain and name.
    pub(crate) fn key(&self) -> base::Network {
        base::Network::new(base::Chain::new(&self.chain), &self.network)
    }
}

/// The network as the SDK builds it.
impl From<&Network> for base::NetworkConfig {
    fn from(network: &Network) -> Self {
        Self {
            network: network.key(),
            chain_id: network.chain_id.clone(),
            testnet: network.testnet,
            block_time: Duration::from_millis(network.block_time_ms),
            confirmations: network.confirmations,
            max_history: network
                .max_history_days
                .map(|days| Duration::from_secs(days.saturating_mul(86_400))),
            explorer_address: network.explorer_address.clone(),
            explorer_transaction: network.explorer_transaction.clone(),
            settings: network.settings.clone(),
            assets: network.assets.iter().map(Into::into).collect(),
            endpoints: network.nodes.clone(),
        }
    }
}

/// The `network` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;
    use anyhow::Context as _;
    use diesel::{prelude::*, sqlite::Sqlite};

    use crate::models::{endpoint, network_asset, network_index, schema::network, uint};

    /// One `network` row as the file holds it.
    #[derive(Queryable, Selectable)]
    #[diesel(table_name = network, check_for_backend(Sqlite))]
    struct Row {
        chain: String,
        #[diesel(column_name = name)]
        network: String,
        chain_id: Option<String>,
        testnet: bool,
        enabled: bool,
        block_time_ms: i64,
        confirmations: i64,
        max_history_days: Option<i64>,
        explorer_address: Option<String>,
        explorer_transaction: Option<String>,
        settings: String,
    }

    impl TryFrom<Row> for Network {
        type Error = anyhow::Error;

        /// The row alone: no assets, nodes or index yet.
        fn try_from(row: Row) -> anyhow::Result<Self> {
            let settings = serde_json::from_str(&row.settings)
                .with_context(|| format!("{}/{}: settings are not JSON", row.chain, row.network))?;
            Ok(Self {
                chain: row.chain,
                network: row.network,
                chain_id: row.chain_id,
                testnet: row.testnet,
                enabled: row.enabled,
                block_time_ms: uint(row.block_time_ms)?,
                confirmations: uint(row.confirmations)?,
                max_history_days: row.max_history_days.map(uint).transpose()?,
                explorer_address: row.explorer_address,
                explorer_transaction: row.explorer_transaction,
                settings,
                assets: Vec::new(),
                endpoints: Vec::new(),
                index: None,
                index_job_reason: None,
                tip_live: None,
                tip_error: None,
                nodes: Vec::new(),
            })
        }
    }

    /// Every network with its assets, nodes and index, in chain, then
    /// name, order.
    ///
    /// # Errors
    ///
    /// The read, or a row that does not check; the refusal never quotes the
    /// stored endpoints.
    pub(crate) fn find_all(connection: &mut SqliteConnection) -> anyhow::Result<Vec<Network>> {
        let rows = network::table
            .select(Row::as_select())
            .order((network::chain, network::name))
            .load(connection)?;
        let mut assets = network_asset::db::find_all(connection)?;
        let endpoints = endpoint::db::find_all(connection)?;
        let mut indexes = network_index::db::find_all(connection)?;
        let mut networks = Vec::new();
        for row in rows {
            let mut network = Network::try_from(row)?;
            let key = network.key();
            network.assets = assets.remove(&key).unwrap_or_default();
            if let Some(nodes) = endpoints.rows().get(&key) {
                network.endpoints = nodes.shown();
                network.nodes = nodes.nodes().to_vec();
            }
            network.index = indexes.remove(&key);
            network.tip_live = network.index.as_ref().and_then(|index| index.tip);
            networks.push(network);
        }
        Ok(networks)
    }
}
