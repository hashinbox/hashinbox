//! What disabled or enabled an RPC node, one `network_endpoint_failure` row
//! each; its newest row, unless `enabled`, disables it
//! (`Endpoint::disabled`): a failure it cannot recover from or a wrong chain
//! (`usecase::nodes::NodeSwitch`), or a disable or an enable by hand
//! (`usecase::networks::set_endpoint_state`). A failure it may recover from (a rate limit, a
//! timeout) is only logged (`rpc.http`).

use super::schema::network_endpoint_failure;
use crate::db::{Keyed, Page};
use diesel::{prelude::*, sqlite::Sqlite};
use serde::{Deserialize, Serialize};

/// One record of one node.
#[derive(
    Clone, Debug, PartialEq, Eq, Queryable, Selectable, Deserialize, Serialize, utoipa::ToSchema,
)]
#[diesel(table_name = network_endpoint_failure, check_for_backend(Sqlite))]
#[schema(as = RpcNodeFailure)]
pub(crate) struct EndpointFailure {
    /// Its id, newest highest; 0 until stored.
    pub(crate) id: i64,
    /// The node's id.
    pub(crate) endpoint_id: i64,
    /// When, Unix ms.
    pub(crate) at: i64,
    /// What: `http_401`, `http_403`, `http_404`, `rpc_{code}` (an error
    /// object of such a code), `wrong_chain` (the node serves another
    /// chain) or `manual` (a disable by hand), each disabling it; or
    /// `enabled` (an enable by hand).
    pub(crate) kind: String,
    /// Why, as shown; never a URL.
    pub(crate) message: String,
}

impl EndpointFailure {
    /// Node `endpoint_id`'s failure of `kind` at `at` (Unix ms), `message`
    /// why.
    pub(crate) fn new(endpoint_id: i64, at: i64, kind: String, message: String) -> Self {
        Self {
            id: 0,
            endpoint_id,
            at,
            kind,
            message,
        }
    }

    /// Node `endpoint_id` found at `at` (Unix ms) serving `served`, not
    /// the network's `expected`.
    pub(crate) fn wrong_chain(endpoint_id: i64, at: i64, served: &str, expected: &str) -> Self {
        let message = format!("the node serves {served}, not this network's {expected}");
        Self::new(endpoint_id, at, "wrong_chain".to_owned(), message)
    }

    /// Node `endpoint_id` disabled by hand at `at` (Unix ms), `reason` why.
    pub(crate) fn manual(endpoint_id: i64, at: i64, reason: String) -> Self {
        Self::new(endpoint_id, at, "manual".to_owned(), reason)
    }

    /// Node `endpoint_id` enabled by hand at `at` (Unix ms), `reason` why.
    pub(crate) fn enabled(endpoint_id: i64, at: i64, reason: String) -> Self {
        Self::new(endpoint_id, at, "enabled".to_owned(), reason)
    }
}

collection! {
    /// Records of nodes, in the order read.
    #[derive(Debug, Serialize)]
    EndpointFailures(EndpointFailure)
}

/// A record's key is its id.
impl Keyed for EndpointFailure {
    fn key(&self) -> String {
        self.id.to_string()
    }
}

/// The `network_endpoint_failure` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    /// Stores `failure`.
    ///
    /// # Errors
    ///
    /// When the write fails.
    pub(crate) fn insert(
        connection: &mut SqliteConnection,
        failure: &EndpointFailure,
    ) -> QueryResult<usize> {
        diesel::insert_into(network_endpoint_failure::table)
            .values((
                network_endpoint_failure::endpoint_id.eq(failure.endpoint_id),
                network_endpoint_failure::at.eq(failure.at),
                network_endpoint_failure::kind.eq(&failure.kind),
                network_endpoint_failure::message.eq(&failure.message),
            ))
            .execute(connection)
    }

    /// A page of node `endpoint_id`'s records, newest first.
    pub(crate) fn find_page(
        connection: &mut SqliteConnection,
        endpoint_id: i64,
        page: Page<i64>,
    ) -> QueryResult<EndpointFailures> {
        let mut query = network_endpoint_failure::table
            .filter(network_endpoint_failure::endpoint_id.eq(endpoint_id))
            .into_boxed();
        if let Some(before) = page.after {
            query = query.filter(network_endpoint_failure::id.lt(before));
        }
        query
            .order(network_endpoint_failure::id.desc())
            .limit(page.limit)
            .select(EndpointFailure::as_select())
            .load(connection)
            .map(Into::into)
    }
}
