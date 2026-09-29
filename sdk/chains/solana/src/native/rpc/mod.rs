//! Solana JSON-RPC: the only code that talks to a node.
//!
//! [`Client`] is generic over the `json_rpc::Client` transport, so the
//! composition root supplies a composed `json_rpc` client and tests supply a
//! scripted double. Capabilities that higher layers
//! hold as trait objects are [`Accounts`] (balances, the batch [`crate::native::Bracket`]),
//! [`Submitter`] (broadcast and its settlement reads), and
//! `transaction::Preflight`; block reads for indexing and the identity checks
//! are inherent methods in `blocks` and `node`.
//!
//! Every call is one attempt: configure the transport's timeout and size
//! limits, and expect no transparent retry or failover.

mod accounts;
mod blocks;
mod client;
mod node;
#[cfg(test)]
pub(crate) mod script;
mod submitter;
mod wire;

pub use accounts::Accounts;
pub use client::Client;
pub use submitter::{SignatureStatus, Submitter};
pub use wire::{Commitment, Reading};
