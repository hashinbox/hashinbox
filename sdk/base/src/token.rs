//! [`Tokens`]: the allowlisted tokens of one network.

use std::{collections::BTreeMap, str::FromStr, sync::Arc};

use crate::{Asset, Network, TransactionError, TransactionErrorKind};

/// The tokens a network may send and read: each by its address in the
/// chain's own type (parsed once, so a block's logs compare without text
/// work), with its decimals. Cheap to clone; clones share the map.
#[derive(Clone, Debug)]
pub struct Tokens<A> {
    network: Network,
    tokens: Arc<BTreeMap<A, u32>>,
}

impl<A: Ord + Copy + FromStr> Tokens<A> {
    /// The `tokens` of `network`, each with its decimals.
    #[must_use]
    pub fn new(network: Network, tokens: impl IntoIterator<Item = (A, u32)>) -> Self {
        Self {
            network,
            tokens: Arc::new(tokens.into_iter().collect()),
        }
    }

    /// Whether `address` is allowlisted.
    #[must_use]
    pub fn contains(&self, address: &A) -> bool {
        self.tokens.contains_key(address)
    }

    /// The decimals of `address`, when allowlisted.
    #[must_use]
    pub fn decimals(&self, address: &A) -> Option<u32> {
        self.tokens.get(address).copied()
    }

    /// Every allowlisted address.
    pub fn iter(&self) -> impl Iterator<Item = A> + '_ {
        self.tokens.keys().copied()
    }

    /// The address of `asset`: `None` for the native coin.
    ///
    /// # Errors
    ///
    /// `Unsupported` for neither the native coin nor an allowlisted token of
    /// this network.
    pub fn contract(&self, asset: &Asset) -> Result<Option<A>, TransactionError> {
        if asset.network != self.network {
            return Err(unsupported());
        }
        if asset.is_native() {
            return Ok(None);
        }
        let address = asset.id.parse().map_err(|_| unsupported())?;
        if !self.contains(&address) {
            return Err(unsupported());
        }
        Ok(Some(address))
    }
}

fn unsupported() -> TransactionError {
    TransactionError::new(
        TransactionErrorKind::Unsupported,
        "not the native coin or an allowlisted token of this network",
    )
}

#[cfg(test)]
mod tests {
    use super::Tokens;
    use crate::{Asset, Chain, Network, TransactionErrorKind};

    fn network(name: &str) -> Network {
        Network::new(Chain::new("test"), name)
    }

    fn tokens() -> Tokens<u32> {
        Tokens::new(network("main"), [(7, 6)])
    }

    fn asset(network_name: &str, id: &str) -> Asset {
        Asset {
            network: network(network_name),
            id: id.to_owned(),
        }
    }

    #[test]
    fn the_native_coin_has_no_contract() {
        assert_eq!(
            tokens().contract(&Asset::native(network("main"))).unwrap(),
            None
        );
    }

    #[test]
    fn an_allowlisted_token_is_its_address() {
        assert_eq!(tokens().contract(&asset("main", "7")).unwrap(), Some(7));
    }

    #[test]
    fn another_network_or_an_unknown_token_is_unsupported() {
        for asset in [
            asset("other", "7"),
            Asset::native(network("other")),
            asset("main", "8"),
            asset("main", "not a number"),
        ] {
            let error = tokens().contract(&asset).unwrap_err();
            assert_eq!(error.kind, TransactionErrorKind::Unsupported);
        }
    }

    #[test]
    fn decimals_are_of_allowlisted_tokens_only() {
        let tokens = tokens();
        assert_eq!(tokens.decimals(&7), Some(6));
        assert_eq!(tokens.decimals(&8), None);
        assert!(tokens.contains(&7) && !tokens.contains(&8));
        assert_eq!(tokens.iter().collect::<Vec<_>>(), [7]);
    }
}
