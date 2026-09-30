//! Sending: [`Transfer`] → [`Preflight`] ([`BuildContext`]) →
//! [`BoundTransfer`] → [`UnsignedTransaction`] → sign →
//! [`SignedTransaction`] → [`crate::native::rpc::Submitter`].
//!
//! The protocol rules a batch needs are here too: consecutive [`Nonces`]
//! per sender and aggregate [`Funds`]. Reserving nonces across sends and
//! settling unknown outcomes is the SDK send pipeline's job, not this
//! module's.

mod builder;
mod fee;
mod funds;
mod id;
mod nonce;
mod preflight;
mod sign;
mod signed;
mod transfer;
mod unsigned;

pub use builder::{BoundTransfer, BuildContext};
pub use fee::FeeLimits;
pub use funds::{Funds, Requirement};
pub use id::TransactionId;
pub use nonce::Nonces;
pub use preflight::Preflight;
pub use signed::{FeeInspection, SignedTransaction};
pub use transfer::Transfer;
pub use unsigned::UnsignedTransaction;
