//! [`PurgedRows`]: what [`crate::IndexStore::purge`] removed.

/// Rows removed by one network rescan, counted by stored collection.
/// Stores with a separate movement collection report its count; stores nesting
/// movements inside transactions remove them with their history rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PurgedRows {
    /// The checkpoint: one when the network had committed a block, zero when
    /// it had not.
    pub checkpoint: u64,
    /// Canonical history entries: one per watched address per transaction.
    pub history: u64,
    /// Value-movement rows, where the backend stores them separately.
    pub movements: u64,
    /// Live (unspent) output rows.
    pub outputs: u64,
    /// Block-credit rows: one per credited address per transactionless credit.
    /// Zero on a chain that has none, which is every chain but Ethereum.
    pub credits: u64,
}
