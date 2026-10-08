//! Ethereum JSON-RPC: the only code that talks to a node.
//!
//! [`Client`] is generic over the `json_rpc::Client` transport, so the
//! composition root supplies a composed `json_rpc` client and tests supply a
//! scripted double. Capabilities that higher layers
//! hold as trait objects are [`Accounts`] (balances, nonces) and [`Submitter`]
//! (broadcast); block reads for indexing are inherent methods in `blocks`.

mod accounts;
mod blocks;
mod call;
mod client;

mod submitter;
mod wire;

pub use accounts::{Accounts, View};
pub use client::Client;
pub use submitter::Submitter;

pub(crate) use call::Remote;
pub(crate) use wire::{parse_fixed_data, parse_quantity_u64};
