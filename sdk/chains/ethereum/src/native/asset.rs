use base::Asset;

use super::{Address, network::MAINNET};

/// Ether metadata: name, ticker, and 18 decimals.
pub const ETH: Asset = Asset::new(MAINNET, "Ethereum", "ETH", 18);

/// Which Ethereum asset a wallet, balance, or movement refers to.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum AssetKind {
    /// Ether itself.
    Native,
    /// An ERC-20 token, identified by its contract address.
    Erc20(Address),
}
