use base::Asset;

use super::network::MAINNET;

/// Bitcoin metadata: name, ticker, and 8 decimals. Bitcoin has one asset.
pub const BTC: Asset = Asset::new(MAINNET, "Bitcoin", "BTC", 8);

/// [`BTC`] at one address, for [`AssetKind::metadata`].
static NATIVE: Asset = BTC;

/// The assets this integration sends and indexes: native BTC only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetKind {
    /// Satoshis moved by transaction inputs and outputs.
    Native,
}

impl AssetKind {
    /// Name, ticker, and decimals of this asset.
    #[must_use]
    pub fn metadata(self) -> &'static Asset {
        match self {
            Self::Native => &NATIVE,
        }
    }
}
