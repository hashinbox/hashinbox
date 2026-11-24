//! The chain and network something belongs to, as one nested object; and
//! each network as stored, one `network_state` view row each: its `network`
//! row with its assets, routes, index and nodes.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Endpoints, NetworkAsset, Route};

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

/// One network's indexing: what its indexer last wrote (its `network_index`
/// row) and why its index job last failed.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = NetworkIndexing)]
pub(crate) struct Index {
    /// The node's newest block at the indexer's last read; always present,
    /// null: no tip known yet (the job failed before its first).
    #[schema(required)]
    pub(crate) tip: Option<u64>,
    /// `tip - confirmations` (saturating), written with it; always present,
    /// null with it.
    #[schema(required)]
    pub(crate) confirmed: Option<u64>,
    /// The last kept block; always present, null: none yet.
    #[schema(required)]
    pub(crate) indexed: Option<u64>,
    /// Unix ms of the indexer's last read whose tip moved; always present.
    #[schema(required)]
    pub(crate) checked_at: Option<u64>,
    /// Blocks kept.
    pub(crate) blocks: u64,
    /// Kept blocks reorged because the chain took them back.
    pub(crate) reorgs: u64,
    /// Failed steps.
    pub(crate) failures: u64,
    /// Why its index job's last run failed or idles (the job's `reason`),
    /// cleared by the next run that does not; always present, null: none.
    #[schema(required)]
    pub(crate) job_reason: Option<String>,
}

/// One network as stored: its `network` row, its `network_asset` rows, its
/// routes, its index and its `network_endpoint` nodes with their counts and
/// state.
#[derive(Clone, Debug, Serialize, utoipa::ToSchema)]
#[schema(as = StoredNetwork)]
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
    /// How many positions below the tip a wallet's start may be; at least
    /// `confirmations`. Always present, null: no limit. Null on a chain that
    /// reads balances (Solana, Ethereum): its line is the chain's final block.
    #[schema(required)]
    pub(crate) max_depth: Option<u64>,
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
    /// Its routed methods, each over its own nodes; every other method goes
    /// to all of them.
    pub(crate) routes: Vec<Route>,
    /// Its indexing; always present, null: no tip known and no index job
    /// reason (or never indexed: not `enabled`).
    #[schema(required)]
    pub(crate) index: Option<Index>,
    /// Its nodes as stored, in failover order; none enabled: no call can go
    /// out, its index job idles and its payments wait.
    #[schema(value_type = Vec<super::Endpoint>)]
    pub(crate) endpoints: Endpoints,
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
            max_depth: network.max_depth,
            explorer_address: network.explorer_address.clone(),
            explorer_transaction: network.explorer_transaction.clone(),
            settings: network.settings.clone(),
            assets: network.assets.iter().map(Into::into).collect(),
        }
    }
}

/// The `network_state` view's statements: the only reads of a network.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;
    use diesel::{prelude::*, sqlite::Sqlite};

    use crate::models::{
        Endpoint, EndpointFailure, EndpointStats, Json, schema::network_state, uint,
    };

    /// One `network_state` row as the file holds it.
    #[derive(Queryable, Selectable)]
    #[diesel(table_name = network_state, check_for_backend(Sqlite))]
    struct Row {
        chain: String,
        network: String,
        chain_id: Option<String>,
        testnet: bool,
        enabled: bool,
        block_time_ms: i64,
        confirmations: i64,
        max_depth: Option<i64>,
        explorer_address: Option<String>,
        explorer_transaction: Option<String>,
        settings: Json<Value>,
        assets: Json<Vec<NetworkAsset>>,
        routes: Json<Vec<Route>>,
        index: Option<Json<Index>>,
        endpoints: Json<Vec<Element>>,
    }

    /// One element of the view's `endpoints`: a node but its network.
    #[derive(Debug, Deserialize)]
    struct Element {
        id: i64,
        url: rpc::Endpoint,
        position: i64,
        calls: i64,
        failed_calls: i64,
        updated_at: i64,
        disabled: Option<EndpointFailure>,
    }

    impl TryFrom<Row> for Network {
        type Error = anyhow::Error;

        fn try_from(row: Row) -> anyhow::Result<Self> {
            let endpoints = row.endpoints.0.into_iter().map(|node| Endpoint {
                id: node.id,
                url: node.url,
                position: node.position,
                stats: EndpointStats {
                    endpoint_id: node.id,
                    calls: node.calls,
                    failed_calls: node.failed_calls,
                    updated_at: node.updated_at,
                },
                disabled: node.disabled,
            });
            Ok(Self {
                chain: row.chain,
                network: row.network,
                chain_id: row.chain_id,
                testnet: row.testnet,
                enabled: row.enabled,
                block_time_ms: uint(row.block_time_ms)?,
                confirmations: uint(row.confirmations)?,
                max_depth: row.max_depth.map(uint).transpose()?,
                explorer_address: row.explorer_address,
                explorer_transaction: row.explorer_transaction,
                settings: row.settings.0,
                assets: row.assets.0,
                routes: row.routes.0,
                index: row.index.map(|index| index.0),
                endpoints: endpoints.collect(),
            })
        }
    }

    /// Every network, in chain, then name, order.
    ///
    /// # Errors
    ///
    /// The read, or a row that does not check; the refusal never quotes the
    /// stored endpoints.
    pub(crate) fn find_all(connection: &mut SqliteConnection) -> anyhow::Result<Vec<Network>> {
        let rows = network_state::table
            .select(Row::as_select())
            .order((network_state::chain, network_state::network))
            .load(connection)?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// `network`; `None`: not stored.
    ///
    /// # Errors
    ///
    /// The read, or a row that does not check.
    pub(crate) fn find(
        connection: &mut SqliteConnection,
        network: &base::Network,
    ) -> anyhow::Result<Option<Network>> {
        let row = network_state::table
            .filter(network_state::chain.eq(network.chain.as_str()))
            .filter(network_state::network.eq(&network.name))
            .select(Row::as_select())
            .first(connection)
            .optional()?;
        row.map(TryInto::try_into).transpose()
    }
}
