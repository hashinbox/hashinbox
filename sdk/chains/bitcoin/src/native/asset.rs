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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::CHAIN_NAME;

    #[test]
    fn native_bitcoin_metadata_is_canonical() {
        assert_eq!(BTC.network.name, CHAIN_NAME);
        assert!(BTC.network.is_mainnet());
        assert_eq!((BTC.name, BTC.ticker, BTC.decimals), ("Bitcoin", "BTC", 8));
        assert_eq!(AssetKind::Native.metadata(), &BTC);
    }
}
