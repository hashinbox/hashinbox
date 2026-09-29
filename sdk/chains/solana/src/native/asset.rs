use base::Asset;

use super::{Address, network::MAINNET};

/// Solana metadata: name, ticker, and 9 decimals (lamports per SOL).
pub const SOL: Asset = Asset::new(MAINNET, "Solana", "SOL", 9);

/// [`SOL`] at one address, for [`AssetKind::metadata`].
static NATIVE: Asset = SOL;

/// Which Solana asset a wallet, balance, or movement refers to.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AssetKind {
    /// Lamports moved by System-program transfers.
    Native,
    /// An SPL token, identified by its mint account.
    SplToken(Address),
}

impl AssetKind {
    /// Name, ticker, and decimals of this asset, for native SOL.
    ///
    /// A mint names no name or ticker on the chain and carries its own
    /// decimals, so an [`AssetKind::SplToken`] has no static metadata: its
    /// decimals travel on the wallet configuration, checked against the mint
    /// account at startup.
    #[must_use]
    pub const fn metadata(&self) -> Option<&'static Asset> {
        match self {
            Self::Native => Some(&NATIVE),
            Self::SplToken(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::CHAIN_NAME;

    #[test]
    fn native_sol_metadata_is_canonical() {
        assert_eq!(SOL.network.name, CHAIN_NAME);
        assert_eq!(SOL.network.native_id, "mainnet-beta");
        assert!(SOL.network.is_mainnet());
        assert_eq!((SOL.name, SOL.ticker, SOL.decimals), ("Solana", "SOL", 9));
        assert_eq!(AssetKind::Native.metadata(), Some(&SOL));
    }

    #[test]
    fn a_mint_carries_identity_without_static_metadata() {
        let mint = Address::from_bytes([3; 32]);
        let asset = AssetKind::SplToken(mint.clone());
        assert_eq!(asset.metadata(), None);
        assert_eq!(asset, AssetKind::SplToken(mint));
        assert_ne!(asset, AssetKind::Native);
    }
}
