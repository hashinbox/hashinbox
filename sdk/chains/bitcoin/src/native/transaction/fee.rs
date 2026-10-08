use serde_json::Number;

use crate::native::{Error, ErrorKind, Satoshi};

/// Virtual bytes per kilo-virtual-byte, the unit Bitcoin Core quotes fee rates in.
const VBYTES_PER_KVB: u64 = 1_000;

/// A fee rate in satoshis per 1,000 virtual bytes (sat/kvB), Bitcoin Core's unit.
///
/// Build one with the unit in the name: [`FeeRate::from_sat_per_vb`] for the
/// wallet-facing sat/vB, [`FeeRate::from_sat_per_kvb`] for Core's own unit.
/// Weight units are never fee units: fees apply to the virtual size.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct FeeRate(u64);

impl FeeRate {
    /// Bitcoin Core's default `maxfeerate`: 0.1 BTC/kvB, i.e. 10,000 sat/vB.
    pub const DEFAULT_MAXIMUM: Self = Self::from_sat_per_vb(10_000);
    /// The highest `maxfeerate` Bitcoin Core accepts: 1 BTC/kvB.
    pub const CORE_LIMIT: Self = Self::from_sat_per_kvb(100_000_000);

    /// `satoshis` per 1,000 virtual bytes.
    #[must_use]
    pub const fn from_sat_per_kvb(satoshis: u64) -> Self {
        Self(satoshis)
    }

    /// `satoshis` per virtual byte; saturates at the `u64` maximum.
    #[must_use]
    pub const fn from_sat_per_vb(satoshis: u64) -> Self {
        Self(satoshis.saturating_mul(VBYTES_PER_KVB))
    }

    /// Satoshis per 1,000 virtual bytes.
    #[must_use]
    pub const fn sat_per_kvb(self) -> u64 {
        self.0
    }

    /// The fee for `virtual_size` virtual bytes, rounded up.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidTransaction`] when the fee exceeds `u64`.
    pub fn fee(self, virtual_size: u64) -> Result<Satoshi, Error> {
        let numerator = u128::from(self.0)
            .checked_mul(u128::from(virtual_size))
            .and_then(|value| value.checked_add(u128::from(VBYTES_PER_KVB) - 1))
            .ok_or_else(|| Error::invalid_transaction("Bitcoin transaction fee overflowed u128"))?;
        u64::try_from(numerator / u128::from(VBYTES_PER_KVB))
            .map(Satoshi::from_atomic)
            .map_err(|_| Error::invalid_transaction("Bitcoin transaction fee overflowed u64"))
    }

    /// This rate as Bitcoin Core's `maxfeerate` argument, in BTC/kvB.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::FeeUnavailable`] for zero or above [`FeeRate::CORE_LIMIT`].
    pub fn core_maximum(self) -> Result<Number, Error> {
        if self.0 == 0 {
            return Err(Error::new(
                ErrorKind::FeeUnavailable,
                "Bitcoin maximum fee rate must be greater than zero",
            ));
        }
        if self > Self::CORE_LIMIT {
            return Err(Error::new(
                ErrorKind::FeeUnavailable,
                "Bitcoin maximum fee rate exceeds Bitcoin Core's 1 BTC/kvB limit",
            ));
        }
        Ok(Satoshi::from_atomic(self.0).btc())
    }
}
