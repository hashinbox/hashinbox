//! The chain and network something belongs to, as one nested object; and
//! each configured network as stored: its `network` row (its indexer's last
//! read of the node) with its endpoints, read from `network_with_endpoints`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::endpoint::RpcNode;

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

/// One configured network: its indexer's last read of the node (`None`
/// until the first) and its endpoints in failover order. `tip_live` is
/// `tip` until a caller that asked the node sets it.
#[derive(Clone, Debug)]
pub(crate) struct Network {
    pub(crate) chain: String,
    pub(crate) network: String,
    /// The node's newest block at the indexer's last read.
    pub(crate) tip: Option<u64>,
    /// `tip - confirmations`.
    pub(crate) confirmed: Option<u64>,
    /// The last kept block.
    pub(crate) indexed: Option<u64>,
    /// Unix ms of the last read whose tip moved.
    pub(crate) checked_at: Option<u64>,
    pub(crate) endpoints: Vec<RpcNode>,
    pub(crate) tip_live: Option<u64>,
}

impl Network {
    /// `network` before its row is written: nothing read, no endpoints.
    pub(crate) fn new(network: &base::Network) -> Self {
        Self {
            chain: network.chain.to_string(),
            network: network.name.clone(),
            tip: None,
            confirmed: None,
            indexed: None,
            checked_at: None,
            endpoints: Vec::new(),
            tip_live: None,
        }
    }
}

/// The `network` table's statements, and `network_with_endpoints`'s.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;
    use diesel::{
        dsl::sql,
        prelude::*,
        sql_types::{BigInt, Nullable},
        sqlite::Sqlite,
        upsert::excluded,
    };
    use serde_json::Value;

    use crate::models::{
        RpcEndpoints, int,
        schema::{network, network_with_endpoints},
        uint,
    };

    /// One `network_with_endpoints` row as the file holds it.
    #[derive(Queryable, Selectable)]
    #[diesel(table_name = network_with_endpoints, check_for_backend(Sqlite))]
    struct Row {
        chain: String,
        network: String,
        tip: Option<i64>,
        confirmed: Option<i64>,
        indexed: Option<i64>,
        checked_at: Option<i64>,
        nodes: Option<String>,
    }

    impl TryFrom<Row> for (base::Network, Network) {
        type Error = anyhow::Error;

        /// The network and its model; a refusal names the network and never
        /// quotes the stored endpoints.
        fn try_from(row: Row) -> anyhow::Result<Self> {
            let endpoints = match &row.nodes {
                None => Vec::new(),
                Some(nodes) => serde_json::from_str::<Vec<Value>>(nodes)
                    .map_err(|_| "stored RPC nodes are not a JSON list".to_owned())
                    .and_then(RpcEndpoints::new)
                    .map_err(|reason| anyhow::anyhow!("{}/{}: {reason}", row.chain, row.network))?
                    .shown(),
            };
            let tip = row.tip.map(uint).transpose()?;
            let key = base::Network::new(base::Chain::new(&row.chain), &row.network);
            Ok((
                key,
                Network {
                    chain: row.chain,
                    network: row.network,
                    tip,
                    confirmed: row.confirmed.map(uint).transpose()?,
                    indexed: row.indexed.map(uint).transpose()?,
                    checked_at: row.checked_at.map(uint).transpose()?,
                    endpoints,
                    tip_live: tip,
                },
            ))
        }
    }

    /// Writes `network`'s row, nothing read yet; keeps one already written.
    ///
    /// # Errors
    ///
    /// When the write fails.
    pub(crate) fn create(
        connection: &mut SqliteConnection,
        network: &base::Network,
    ) -> anyhow::Result<()> {
        diesel::insert_into(network::table)
            .values((
                network::chain.eq(network.chain.as_str()),
                network::name.eq(&network.name),
            ))
            .on_conflict_do_nothing()
            .execute(connection)?;
        Ok(())
    }

    /// Keeps `network`'s `tip`, its `confirmed` position and the `indexed`
    /// one (`None`: keeps the stored one). `checked_at` becomes `now` (Unix
    /// ms) only when the tip moved: a node that stopped producing does not
    /// look fresh.
    ///
    /// # Errors
    ///
    /// When a value is out of range or the write fails.
    pub(crate) fn save(
        connection: &mut SqliteConnection,
        network: &base::Network,
        tip: u64,
        confirmed: u64,
        indexed: Option<u64>,
        now: u64,
    ) -> anyhow::Result<()> {
        diesel::insert_into(network::table)
            .values((
                network::chain.eq(network.chain.as_str()),
                network::name.eq(&network.name),
                network::tip.eq(int(tip)?),
                network::confirmed.eq(int(confirmed)?),
                network::indexed.eq(indexed.map(int).transpose()?),
                network::checked_at.eq(int(now)?),
            ))
            .on_conflict((network::chain, network::name))
            .do_update()
            .set((
                network::tip.eq(excluded(network::tip)),
                network::confirmed.eq(excluded(network::confirmed)),
                network::indexed.eq(sql::<Nullable<BigInt>>(
                    "COALESCE(excluded.indexed, network.indexed)",
                )),
                network::checked_at.eq(sql::<Nullable<BigInt>>(
                    "CASE WHEN excluded.tip IS NOT network.tip \
                     THEN excluded.checked_at ELSE network.checked_at END",
                )),
            ))
            .execute(connection)?;
        Ok(())
    }

    /// Every stored network with its endpoints.
    ///
    /// # Errors
    ///
    /// The read, or a row that no longer checks; the refusal never quotes
    /// its endpoints.
    pub(crate) fn find_all(
        connection: &mut SqliteConnection,
    ) -> anyhow::Result<BTreeMap<base::Network, Network>> {
        let rows = network_with_endpoints::table
            .select(Row::as_select())
            .load(connection)?;
        rows.into_iter().map(TryInto::try_into).collect()
    }
}
