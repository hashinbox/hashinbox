//! The server's own decorators of a stored RPC node, beside `rpc`'s:
//! [`Guard`] turns a node that cannot recover off, [`Counter`] counts its
//! requests in `network_endpoint_stats`.

mod counter;
mod guard;

pub(crate) use counter::Counter;
pub(crate) use guard::Guard;
