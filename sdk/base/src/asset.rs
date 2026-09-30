use crate::{ChainName, ChainNetwork};

/// One asset within one chain family; the chain's own currency is [`AssetId::native`].
///
/// It names no network, on purpose. The chain's own currency is one asset on
/// every network of that chain, and a chain builds a token's identity from the
/// chain name and the token's own text alone. Which network an amount of it
/// sits on is said by the value beside it: the [`crate::Address`] that holds
/// it or the [`crate::TransactionRef`] that moved it. Whoever pairs the two
/// checks only that the chains agree.
///
/// The same token text on two networks of one chain is therefore one
/// `AssetId`: a map keyed by `AssetId` alone cannot tell them apart.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AssetId {
    /// The chain family the asset lives on.
    pub chain: ChainName,
    /// Chain-owned asset text: `native`, or, e.g., a token contract address.
    pub asset: String,
}

/// Asset text naming a chain's own currency.
const NATIVE: &str = "native";

impl AssetId {
    /// The chain's own currency (BTC, ETH, SOL).
    #[must_use]
    pub fn native(chain: ChainName) -> Self {
        Self {
            chain,
            asset: NATIVE.to_owned(),
        }
    }

    /// Whether this names the chain's own currency.
    #[must_use]
    pub fn is_native(&self) -> bool {
        self.asset == NATIVE
    }
}

/// Metadata describing an asset on one concrete blockchain network.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Asset {
    /// The network the asset is issued on.
    pub network: ChainNetwork,
    /// Human-readable name, such as `Bitcoin`.
    pub name: &'static str,
    /// Display symbol, such as `BTC`.
    pub ticker: &'static str,
    /// Fractional digits between the display unit and the atomic unit.
    pub decimals: u32,
}

impl Asset {
    /// Declares an asset as a `const`.
    #[must_use]
    pub const fn new(
        network: ChainNetwork,
        name: &'static str,
        ticker: &'static str,
        decimals: u32,
    ) -> Self {
        Self {
            network,
            name,
            ticker,
            decimals,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NetworkKind;

    #[test]
    fn native_asset_is_named_once_per_chain() {
        let native = AssetId::native(ChainName::new("test"));

        assert_eq!(native.chain, ChainName::new("test"));
        assert_eq!(native.asset, "native");
        assert!(native.is_native());
        assert!(
            !AssetId {
                asset: "token".to_owned(),
                ..native
            }
            .is_native()
        );
    }

    #[test]
    fn asset_carries_its_chain_metadata() {
        let example = ChainName::from_static("example");
        let network = ChainNetwork::new(example.clone(), "1", NetworkKind::Mainnet, "EX");
        let asset = Asset::new(network, "Example USD", "xUSD", 6);

        assert_eq!(asset.network.name, example);
        assert_eq!(asset.name, "Example USD");
        assert_eq!(asset.ticker, "xUSD");
        assert_eq!(asset.decimals, 6);
        assert_eq!(asset.network.native_id, "1");
    }
}
