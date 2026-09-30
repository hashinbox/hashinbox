//! Bitcoin Core JSON-RPC: the only code that talks to a node.
//!
//! [`Client`] is generic over the `json_rpc::Client` transport, so the
//! composition root supplies a composed `json_rpc` client and tests supply a
//! scripted double. Capabilities that higher layers
//! hold as trait objects are [`Submitter`] (broadcast and lookup) and
//! `transaction::Preflight` (fee ceiling and `testmempoolaccept`); fee
//! estimation, block reads for indexing, and the readiness check are inherent
//! methods in `fees`, `blocks`, and `node`.

mod blocks;
mod client;
mod fees;
mod node;
#[cfg(test)]
pub(crate) mod script;
mod submitter;
pub(crate) mod wire;

#[cfg(test)]
pub(crate) use blocks::tests as fixture;
pub use client::Client;
pub use node::Verification;
pub use submitter::Submitter;
