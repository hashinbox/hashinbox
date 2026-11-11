//! Test doubles (feature `testing`): the two implementations a test of the
//! engine needs, held in memory and fully offline.
//!
//! - [`Wallets`]: a [`Store`](crate::Store), the reference
//!   for what each [`Selector`](crate::selection::Selector) means.
//! - [`Fees`]: a [`FeeEstimator`](base::FeeEstimator) at fixed prices that
//!   keeps every set of spends it was asked to price.

mod fee;
mod memory;

pub use fee::Fees;
pub use memory::Wallets;
