//! The indexes' writes that span several models: a wallet index's legs
//! ([`wallet`]) and a payment's transaction rows ([`transaction`]).

pub(crate) mod transaction;
pub(crate) mod wallet;
