//! The codes (`target`s) of the events this crate logs; their messages are
//! the server's catalog (`apps/server/logs.json`).

/// A request answered, no error object.
pub const RPC_HTTP_ANSWERED: &str = "rpc.http.answered";
/// A request answered: the node is not at the block asked yet.
pub const RPC_HTTP_NOT_YET: &str = "rpc.http.not_yet";
/// A request failed: a transport error or an error object.
pub const RPC_HTTP_FAILED: &str = "rpc.http.failed";
/// A request the node broke on (HTTP 5xx).
pub const RPC_HTTP_BROKEN: &str = "rpc.http.broken";
/// A request still pending.
pub const RPC_SLOW: &str = "rpc.slow";
/// A call repeated.
pub const RPC_RETRY: &str = "rpc.retry";
/// A request that waited for its rate.
pub const RPC_WAIT: &str = "rpc.wait";
