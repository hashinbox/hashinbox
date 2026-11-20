//! Test doubles (feature `testing`), held in memory and fully offline:
//! [`Wallets`], a [`Store`](crate::Store), the reference for what each
//! [`Selector`](crate::selection::Selector) means. A fixed-price fee
//! estimator is base's (`base::testing::Fees`).

mod memory;

pub use memory::Wallets;
