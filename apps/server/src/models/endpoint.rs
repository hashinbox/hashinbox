//! Every network's RPC endpoints, in the server's `rpc_endpoints` table of the
//! one database file: seeded from the configuration file once, the primary
//! source after that.

use std::fmt;

use base::{ChainName, Network};
use serde_json::Value;
use sqlx::{Error, SqlitePool as Pool};

/// The most endpoints one network may list.
const MOST: usize = 16;

/// One configured RPC endpoint: the document a configuration file's
/// `rpc.endpoints` holds — a URL, or an object with the URL and bounds —
/// checked as `json_rpc::Endpoint` checks it.
///
/// The URL may carry credentials (userinfo, query keys, a path key). It is
/// stored as written and leaves this value only as
/// [`RpcEndpoints::documents`]; what is shown is [`Self::origin`], which is all
/// `Display` and `Debug` print.
#[derive(Clone)]
pub(crate) struct Endpoint {
    document: Value,
    /// The node the document describes.
    endpoint: json_rpc::Endpoint,
    /// The URL's scheme, host and port; `unresolved` when it does not parse.
    origin: String,
}

/// One row of `rpc_endpoints`: a network's endpoints in failover order, one
/// to [`MOST`] of them. The store's operations are this row's.
#[derive(Clone)]
pub(crate) struct RpcEndpoints(Vec<Endpoint>);

/// One `rpc_endpoints` row as the file holds it, before it is checked.
#[derive(sqlx::FromRow)]
struct Row {
    chain: String,
    network: String,
    nodes: String,
}

impl TryFrom<Row> for (Network, RpcEndpoints) {
    type Error = String;

    /// The network and its endpoints; a refusal names the network and never
    /// quotes the stored documents.
    fn try_from(row: Row) -> Result<Self, String> {
        let endpoints = serde_json::from_str::<Vec<Value>>(&row.nodes)
            .map_err(|_| "stored RPC nodes are not a JSON list".to_owned())
            .and_then(RpcEndpoints::new)
            .map_err(|reason| format!("{}/{}: {reason}", row.chain, row.network))?;
        Ok((
            Network::new(ChainName::new(&row.chain), row.network),
            endpoints,
        ))
    }
}

impl Endpoint {
    /// Checks `document` without contacting a node.
    ///
    /// # Errors
    ///
    /// A message that never quotes the document: one that is neither a URL
    /// nor an endpoint object, or one `json_rpc::Endpoint::validate` refuses.
    pub(crate) fn new(document: Value) -> Result<Self, String> {
        let endpoint: json_rpc::Endpoint = serde_json::from_value(document.clone())
            .map_err(|_| "an RPC node is a URL or an object of url and bounds")?;
        endpoint.validate().map_err(|error| error.to_string())?;
        let origin = url::Url::parse(&endpoint.url).map_or_else(
            |_| "unresolved".to_owned(),
            |url| url.origin().ascii_serialization(),
        );
        Ok(Self {
            document,
            endpoint,
            origin,
        })
    }

    /// What may be shown of the endpoint: user information, path and query
    /// are dropped, because any of them may hold a credential.
    pub(crate) fn origin(&self) -> &str {
        &self.origin
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.origin)
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("Endpoint")
            .field(&self.origin)
            .finish()
    }
}

impl RpcEndpoints {
    /// Every document checked, in order.
    ///
    /// # Errors
    ///
    /// A message naming the position of the first unusable endpoint and never
    /// its contents, or the list's size when it is empty or longer than
    /// [`MOST`].
    pub(crate) fn new(documents: Vec<Value>) -> Result<Self, String> {
        if documents.is_empty() || documents.len() > MOST {
            return Err(format!("a network lists between 1 and {MOST} RPC nodes"));
        }
        let endpoints = documents
            .into_iter()
            .enumerate()
            .map(|(position, document)| {
                Endpoint::new(document).map_err(|reason| format!("RPC node {position}: {reason}"))
            });
        endpoints.collect::<Result<_, _>>().map(Self)
    }

    /// The documents, in failover order, for storage and for the section a
    /// chain parses them in.
    pub(crate) fn documents(&self) -> Vec<Value> {
        self.0
            .iter()
            .map(|endpoint| endpoint.document.clone())
            .collect()
    }

    /// The endpoints, in failover order.
    pub(crate) fn endpoints(&self) -> &[Endpoint] {
        &self.0
    }

    /// The nodes the documents describe, in failover order, for a transport.
    pub(crate) fn nodes(&self) -> Vec<json_rpc::Endpoint> {
        self.0
            .iter()
            .map(|endpoint| endpoint.endpoint.clone())
            .collect()
    }

    /// The stored endpoints of every network, one row each.
    ///
    /// # Errors
    ///
    /// The read, or a row that no longer checks; the refusal never quotes it.
    pub(crate) async fn find_all(pool: &Pool) -> Result<Vec<(Network, Self)>, Error> {
        let rows: Vec<Row> = sqlx::query_as("SELECT chain, network, nodes FROM rpc_endpoints")
            .fetch_all(pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                row.try_into()
                    .map_err(|reason: String| Error::Decode(reason.into()))
            })
            .collect()
    }

    /// Stores `endpoints` for `network` unless it already has a row: the
    /// configuration file's endpoints are the bootstrap, never an override.
    pub(crate) async fn insert_missing(
        pool: &Pool,
        network: &Network,
        endpoints: &Self,
    ) -> Result<(), Error> {
        Self::write(
            pool,
            "INSERT INTO rpc_endpoints (chain, network, nodes) VALUES (?1, ?2, ?3)
             ON CONFLICT (chain, network) DO NOTHING",
            network,
            endpoints,
        )
        .await
    }

    /// Makes `endpoints` the stored ones of `network`.
    pub(crate) async fn upsert(
        pool: &Pool,
        network: &Network,
        endpoints: &Self,
    ) -> Result<(), Error> {
        Self::write(
            pool,
            "INSERT INTO rpc_endpoints (chain, network, nodes) VALUES (?1, ?2, ?3)
             ON CONFLICT (chain, network) DO UPDATE SET nodes = excluded.nodes",
            network,
            endpoints,
        )
        .await
    }

    /// One statement, atomic on its own.
    async fn write(
        pool: &Pool,
        sql: &'static str,
        network: &Network,
        endpoints: &Self,
    ) -> Result<(), Error> {
        let documents = serde_json::to_string(&endpoints.documents())
            .map_err(|_| Error::Encode("RPC nodes did not encode".into()))?;
        sqlx::query(sql)
            .bind(network.chain.as_str())
            .bind(&network.name)
            .bind(documents)
            .execute(pool)
            .await
            .map(drop)
    }
}

impl fmt::Debug for RpcEndpoints {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_list().entries(&self.0).finish()
    }
}
