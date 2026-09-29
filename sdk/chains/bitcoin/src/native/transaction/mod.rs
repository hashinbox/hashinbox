//! Sending: [`Transfer`] + [`BuildContext`] → [`Builder`] (or
//! [`Builder::grouped`] over one [`Funds`] per source, for several) →
//! [`UnsignedTransaction`] → sign → [`SignedTransaction`] → [`Preflight`]
//! (`testmempoolaccept`) → [`crate::native::rpc::Submitter`].
//!
//! Coin selection is pure: the spendable outputs come from the index, and
//! which of them a send may take is the SDK send pipeline's job, not this
//! module's.

mod builder;
mod fee;
mod funds;
mod id;
mod preflight;
mod sighash;
mod sign;
mod signed;
mod transfer;
mod unsigned;

pub use builder::{BuildContext, Builder};
pub use fee::FeeRate;
pub use funds::Funds;
pub use id::TransactionId;
pub use preflight::{Acceptance, Preflight};
pub use sighash::SighashType;
pub use signed::SignedTransaction;
pub use transfer::Transfer;
pub use unsigned::{Spend, UnsignedTransaction};
