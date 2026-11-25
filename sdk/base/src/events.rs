//! The codes (`target`s) of the events this crate logs; their messages are
//! the server's catalog (`apps/server/logs.json`).

/// An index step finished.
pub const INDEX_STEP: &str = "index.step";
/// Kept blocks reorged.
pub const INDEX_REORG: &str = "index.reorg";
