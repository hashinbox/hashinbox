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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_asset_decides_the_call_target_and_attached_value() {
        let native = Transfer {
            asset: AssetKind::Native,
            to: Address::from_bytes([0x22; 20]),
            amount: Wei::from_u128(7),
        };
        assert_eq!(
            (native.target(), native.value()),
            (&Address::from_bytes([0x22; 20]), Wei::from_u128(7))
        );
        let token = Transfer {
            asset: AssetKind::Erc20(Address::from_bytes([0x33; 20])),
            ..native
        };
        assert_eq!(
            (token.target(), token.value()),
            (&Address::from_bytes([0x33; 20]), Wei::ZERO)
        );
    }
}
