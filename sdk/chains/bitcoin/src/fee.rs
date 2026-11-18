//! [`FeeRate`]: Bitcoin's fee rate, and the [`vsize`] of a transaction it
//! prices.

use bitcoin::Script;

/// Weight of an input without its witness: outpoint, empty script, sequence.
const INPUT_WEIGHT: u64 = 4 * (32 + 4 + 1 + 4);
/// Version, lock time, one-byte counts, segwit marker and flag.
const BASE_WEIGHT: u64 = 4 * (4 + 4 + 1 + 1) + 2;
/// Witness weight spending P2WPKH (signature and key) and P2TR key path.
const P2WPKH_WITNESS: u64 = 109;
const P2TR_WITNESS: u64 = 67;

/// A fee rate in satoshis per 1,000 virtual bytes, Bitcoin Core's unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FeeRate(u64);

impl FeeRate {
    /// Bitcoin Core's default `maxfeerate`: 0.1 BTC/kvB.
    pub const DEFAULT_MAXIMUM: Self = Self(10_000_000);
    /// The highest `maxfeerate` Bitcoin Core accepts: 1 BTC/kvB.
    pub const CORE_LIMIT: Self = Self(100_000_000);

    /// `satoshis` per 1,000 virtual bytes.
    #[must_use]
    pub const fn from_sat_per_kvb(satoshis: u64) -> Self {
        Self(satoshis)
    }

    /// Satoshis per 1,000 virtual bytes.
    #[must_use]
    pub const fn sat_per_kvb(self) -> u64 {
        self.0
    }

    /// The fee for `virtual_size` virtual bytes, rounded up.
    #[must_use]
    pub const fn fee(self, virtual_size: u64) -> u64 {
        (self.0 * virtual_size).div_ceil(1_000)
    }
}

impl From<base::FeeRate> for FeeRate {
    /// Its total, `base + tip`.
    fn from(rate: base::FeeRate) -> Self {
        Self(u64::try_from(rate.total()).unwrap_or(u64::MAX))
    }
}

/// Virtual size of a transaction spending outputs that pay `inputs` (each
/// P2WPKH or P2TR) and paying `outputs`.
pub(crate) fn vsize<'a>(
    inputs: impl IntoIterator<Item = &'a Script>,
    outputs: impl IntoIterator<Item = &'a Script>,
) -> u64 {
    let weight = BASE_WEIGHT
        + inputs.into_iter().map(input_weight).sum::<u64>()
        + outputs.into_iter().map(output_weight).sum::<u64>();
    weight.div_ceil(4)
}

/// Weight of an input spending `script`, its witness included.
fn input_weight(script: &Script) -> u64 {
    INPUT_WEIGHT
        + if script.is_p2tr() {
            P2TR_WITNESS
        } else {
            P2WPKH_WITNESS
        }
}

/// Weight of an output paying `script`: value, length prefix, script.
fn output_weight(script: &Script) -> u64 {
    let length = script.len() as u64;
    let prefix = if length < 0xfd { 1 } else { 3 };
    (8 + prefix + length) * 4
}
