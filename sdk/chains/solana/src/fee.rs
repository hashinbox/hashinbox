//! The [`Priority`] a Solana transaction buys at a `base::FeeRate`, its
//! compute-unit [`limit`] and its [`fee`].

use base::TransactionError;

/// Compute units a SOL transfer may use: about twice what it does, the
/// transfer (150), its 44-byte memo (about 16,000: the Memo program spends
/// some 350 a byte) and the two ComputeBudget instructions (150 each).
const SOL_UNITS: u32 = 40_000;

/// Compute units a token transfer may use: about twice what it does when it
/// creates the recipient's token account (about 25,000), plus
/// `TransferChecked` (about 6,500), the memo and the ComputeBudget
/// instructions. It always carries the idempotent create, so one limit
/// covers a recipient with an account too.
const TOKEN_UNITS: u32 = 100_000;

/// Micro-lamports per lamport.
const MICRO_LAMPORTS: u128 = 1_000_000;

/// The lowest price per compute unit sent, in micro-lamports.
pub(crate) const FLOOR: u64 = 1;

/// The fee of a transaction of `signatures` at `per_signature` lamports
/// paying `priority`.
pub(crate) fn fee(
    per_signature: u64,
    signatures: u8,
    priority: Priority,
) -> Result<u64, TransactionError> {
    per_signature
        .checked_mul(u64::from(signatures))
        .zip(priority.fee())
        .and_then(|(signed, priority)| signed.checked_add(priority))
        .ok_or_else(|| TransactionError::failed("Solana fee overflows"))
}

/// What a transaction pays to be scheduled ahead of others: the compute
/// units it may use (its ComputeBudget limit) at a price per unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Priority {
    /// Compute units it may use.
    pub(crate) limit: u32,
    /// Micro-lamports per compute unit.
    pub(crate) price: u64,
}

impl Priority {
    /// Lamports: `ceil(price × limit / 1_000_000)`; `None` above a `u64`.
    fn fee(self) -> Option<u64> {
        let micro = u128::from(self.price) * u128::from(self.limit);
        u64::try_from(micro.div_ceil(MICRO_LAMPORTS)).ok()
    }
}

/// The compute-unit limit of a transfer of a token, or of SOL.
pub(crate) const fn limit(token: bool) -> u32 {
    if token { TOKEN_UNITS } else { SOL_UNITS }
}
