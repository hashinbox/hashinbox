//! Every RPC node's counts, one `network_endpoint_stats` row each, made with
//! its node (`init.sql`), added to as each request finishes
//! (`usecase::decorators::Counter`).

use serde::Serialize;

/// One node's counts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[schema(as = RpcNodeStats)]
pub(crate) struct EndpointStats {
    /// The node's id.
    pub(crate) endpoint_id: i64,
    /// HTTP requests that reached it and finished.
    pub(crate) calls: i64,
    /// Of those, the ones that failed.
    pub(crate) failed_calls: i64,
    /// Unix ms of the last; when the node was stored if none.
    pub(crate) updated_at: i64,
}

/// The `network_endpoint_stats` table's statements.
pub(crate) mod db {
    use diesel::prelude::*;

    use crate::models::schema::network_endpoint_stats::dsl::{
        calls, failed_calls, network_endpoint_stats, updated_at,
    };

    /// Counts one call of node `endpoint_id` at `at` (Unix ms), `failed`
    /// or not. A node no longer stored has no row: nothing.
    ///
    /// # Errors
    ///
    /// The write.
    pub(crate) fn add_call(
        connection: &mut SqliteConnection,
        endpoint_id: i64,
        failed: bool,
        at: i64,
    ) -> QueryResult<usize> {
        diesel::update(network_endpoint_stats.find(endpoint_id))
            .set((
                calls.eq(calls + 1),
                failed_calls.eq(failed_calls + i64::from(failed)),
                updated_at.eq(at),
            ))
            .execute(connection)
    }
}
