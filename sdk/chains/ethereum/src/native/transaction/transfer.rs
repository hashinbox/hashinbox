use crate::native::{Address, AssetKind, Wei};

/// One requested payment, in native types: which asset, to whom, how much.
/// Never an observed movement. The sender is whoever signs it.
///
/// Only ether and ERC-20 `transfer` calls are expressible; arbitrary
/// calldata is not.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Transfer {
    /// Ether, or the ERC-20 token whose `transfer` is called.
    pub asset: AssetKind,
    /// The credited account.
    pub to: Address,
    /// Wei for ether; the token's smallest unit for an ERC-20 token.
    pub amount: Wei,
}

impl Transfer {
    /// The transaction's `to` field: the recipient, or the token contract.
    #[must_use]
    pub const fn target(&self) -> &Address {
        match &self.asset {
            AssetKind::Native => &self.to,
            AssetKind::Erc20(token) => token,
        }
    }

    /// The ether attached to the transaction; zero for ERC-20 calls.
    #[must_use]
    pub fn value(&self) -> Wei {
        match self.asset {
            AssetKind::Native => self.amount.clone(),
            AssetKind::Erc20(_) => Wei::ZERO,
        }
    }
}
