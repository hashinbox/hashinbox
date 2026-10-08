#![doc = include_str!("../README.md")]

/// The configuration document, the registry that hands each named section to
/// the chain registered for it, and the composition of every registered chain.
pub mod config;
mod networks;
mod payments;

pub use networks::{NetworkError, NetworkState, Networks};
pub use payments::{Builder, Payments, Split, StartError};
