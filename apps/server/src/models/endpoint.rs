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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::models::tests::File;

    fn network() -> Network {
        Network::new(ChainName::new("fixture"), "one")
    }

    fn endpoints(url: &str) -> RpcEndpoints {
        RpcEndpoints::new(vec![json!(url)]).expect("a usable endpoint")
    }

    async fn origins(pool: &Pool) -> Vec<String> {
        RpcEndpoints::find_all(pool)
            .await
            .expect("readable")
            .into_iter()
            .find(|(stored, _)| *stored == network())
            .map(|(_, list)| list.endpoints().iter().map(ToString::to_string).collect())
            .unwrap_or_default()
    }

    /// Everything that may hold a credential stays out of what is shown.
    #[test]
    fn only_the_origin_of_each_endpoint_is_shown() {
        let list = RpcEndpoints::new(vec![
            json!("http://user:s3cret@127.0.0.1:18443/wallet/s3cret?apikey=s3cret"),
            json!({"url": "https://user:s3cret@node.invalid/v3/s3cret",
                   "bounds": {"timeout_ms": 500}}),
            json!("https://other.invalid/v1"),
        ])
        .expect("three usable endpoints");
        let shown: Vec<&str> = list.endpoints().iter().map(Endpoint::origin).collect();
        assert_eq!(
            shown,
            [
                "http://127.0.0.1:18443",
                "https://node.invalid",
                "https://other.invalid"
            ]
        );
        let first = r#"[Endpoint("http://127.0.0.1:18443"), Endpoint("https://node.invalid"), "#;
        assert_eq!(
            format!("{list:?}"),
            format!(r#"{first}Endpoint("https://other.invalid")]"#)
        );
        assert_eq!(list.endpoints()[0].to_string(), "http://127.0.0.1:18443");
        assert_eq!(
            list.documents().len(),
            3,
            "the documents are kept as written"
        );
    }

    /// Every malformed document is refused at the constructor, by position and
    /// without quoting it.
    #[test]
    fn an_unusable_list_is_refused_without_echoing_it() {
        for (documents, expected) in [
            (vec![], "a network lists between 1 and 16 RPC nodes"),
            (
                vec![json!("http://ok.invalid"); 17],
                "a network lists between 1 and 16 RPC nodes",
            ),
            (
                vec![json!("http://ok.invalid"), json!(7)],
                "RPC node 1: an RPC node is a URL or an object of url and bounds",
            ),
            (
                vec![json!({"url": "http://s3cret.invalid", "extra": "s3cret"})],
                "RPC node 0: an RPC node is a URL or an object of url and bounds",
            ),
            (
                vec![json!("ws://s3cret.invalid")],
                "RPC node 0: JSON-RPC endpoint scheme must be http or https",
            ),
            (
                vec![json!({"url": "http://s3cret.invalid", "bounds": {"timeout_ms": 0}})],
                "RPC node 0: JSON-RPC endpoint `timeout_ms` must be a positive integer",
            ),
        ] {
            let refused = RpcEndpoints::new(documents).expect_err("refused");
            assert_eq!(refused, expected);
            assert!(!refused.contains("s3cret"));
        }
    }

    /// A seed never overrides a stored row; a replacement always does, and
    /// both survive reopening, documents identical.
    #[tokio::test]
    async fn the_first_seed_is_kept_until_an_edit_replaces_it() {
        let file = File::new();
        let pool = file.pool().await;
        assert!(origins(&pool).await.is_empty(), "nothing stored yet");
        let seeded = endpoints("http://first.invalid");
        RpcEndpoints::insert_missing(&pool, &network(), &seeded)
            .await
            .expect("seeded");
        let ignored = endpoints("http://second.invalid");
        RpcEndpoints::insert_missing(&pool, &network(), &ignored)
            .await
            .expect("kept");
        assert_eq!(origins(&pool).await, ["http://first.invalid"]);
        let edited = RpcEndpoints::new(vec![json!({"url": "http://edited.invalid/v3/k3y",
            "bounds": {"timeout_ms": 500}})])
        .expect("a usable endpoint");
        RpcEndpoints::upsert(&pool, &network(), &edited)
            .await
            .expect("edited");
        pool.close().await;

        let pool = file.pool().await;
        let third = endpoints("http://third.invalid");
        RpcEndpoints::insert_missing(&pool, &network(), &third)
            .await
            .expect("kept");
        let read = RpcEndpoints::find_all(&pool).await.expect("readable");
        let documents: Vec<_> = read
            .iter()
            .map(|(stored, list)| (stored, list.documents()))
            .collect();
        assert_eq!(documents, [(&network(), edited.documents())]);
    }

    /// A row that no longer checks is refused, and the refusal does not quote it.
    #[tokio::test]
    async fn an_unusable_stored_row_is_refused_without_echoing_it() {
        let file = File::new();
        let pool = file.pool().await;
        sqlx::raw_sql(
            "INSERT INTO rpc_endpoints VALUES ('fixture', 'one', '[\"ws://s3cret.invalid\"]')",
        )
        .execute(&pool)
        .await
        .expect("written");
        let refused = RpcEndpoints::find_all(&pool)
            .await
            .expect_err("refused")
            .to_string();
        assert!(refused.contains(": fixture/one: RPC node 0"), "{refused}");
        assert!(!refused.contains("s3cret"));
    }
}
