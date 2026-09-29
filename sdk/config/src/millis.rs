//! One place that turns a document's `_ms` key into a [`Duration`].
//!
//! Every wait in this crate is written in milliseconds by an operator and held
//! as a `Duration` in Rust, so a caller that builds a value in code never
//! multiplies anything and a caller that reads a file never sees a bare
//! integer travel further than this module.

use std::time::Duration;

use serde::{Deserialize as _, Deserializer};

/// Reads a non-negative integer count of milliseconds.
pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Duration::from_millis(u64::deserialize(deserializer)?))
}
