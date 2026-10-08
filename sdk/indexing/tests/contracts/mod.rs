//! Repository contract scenarios every store runs.
//!
//! `repository_contract.rs` builds a [`WatchContract`], a [`HistoryContract`], a
//! [`BalanceContract`], a [`BackfillContract`], a [`SpendContract`], a
//! [`PurgeContract`] or a [`WalletContract`] over an empty repository and
//! awaits one scenario per test; each scenario writes its fixture first and
//! panics on the first contract violation.
//!
//! [`WatchContract`] is the one every store must pass. It and the commit
//! contracts are the *minimum*: the rest state what a store owes once it offers
//! a consumer capability, and a sink that offers none of them still indexes
//! (`tests/minimum_contract.rs` is that proof).
//!
//! Every scenario runs against **two** stores — `indexing::sqlite::Repository`
//! on a file and `indexing::memory::Repository` on maps — which is what keeps
//! them a statement about the ports rather than one backend's own test names,
//! and is how a divergence between the two is caught at all.
//!
//! **These scenarios are integration-test code, not crate API.** They used to be
//! `indexing::testing` behind a cargo feature, which made a consumer that wanted
//! only the in-memory store link the whole suite; that store is now
//! `indexing::memory`, shipped on its own feature, and the suite is here, where
//! nothing outside `sdk/indexing/tests/` can reach it. `src/` therefore cannot
//! run them either, which is why SQLite's adapter modules keep only what is
//! SQLite's own.

mod agreement;
mod backfill;
mod backfill_fixture;
mod fixture;
mod idempotent_replay;
mod purge;
mod rollback;
mod unmatched_spends;
mod wallets;
mod watch;

pub use self::wallets::WalletContract;
pub use crate::rows::Rows;
pub use backfill::BackfillContract;
pub use purge::PurgeContract;
pub use unmatched_spends::SpendContract;
pub use watch::WatchContract;
