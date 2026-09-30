//! What a `base::Network` must admit before it may be used as an index
//! network: the chain-family check every component makes, and the strict
//! conversion of caller addresses into a chain's native address set.
//!
//! Two free functions rather than a trait. Both take the network as their
//! first argument because `IndexError` lives here and `Network` lives in
//! `base`, so neither can be an inherent method. A one-method-per-caller trait
//! with exactly one implementation bought method syntax and nothing else, and
//! `rules."rust/free-function".mode = "receiver"` was checked against this
//! shape and does not forbid it.

use std::collections::BTreeSet;

use base::{Address, ChainName, Network};

use crate::{IndexError, IndexErrorKind};

/// Accepts `network` for a component of the `chain` family.
///
/// # Errors
///
/// [`IndexErrorKind::NetworkMismatch`] for another chain or a blank network.
pub fn require_chain(network: &Network, chain: &ChainName) -> Result<(), IndexError> {
    if network.chain != *chain {
        return Err(IndexError::new(
            IndexErrorKind::NetworkMismatch,
            format!("network chain {} is not {chain}", network.chain),
            false,
        ));
    }
    if network.name.trim().is_empty() {
        return Err(IndexError::new(
            IndexErrorKind::NetworkMismatch,
            format!("{chain} network name must not be empty"),
            false,
        ));
    }
    Ok(())
}

/// Converts caller-selected `addresses` into `network`'s native address set,
/// strictly (exact canonical text), so an interpreter can test membership
/// natively.
///
/// # Errors
///
/// [`IndexErrorKind::NetworkMismatch`] when an address belongs to another
/// network and [`IndexErrorKind::InvalidRequest`] when `A` rejects its text.
pub fn select_addresses<A>(
    network: &Network,
    addresses: &[Address],
) -> Result<BTreeSet<A>, IndexError>
where
    A: for<'a> TryFrom<&'a Address> + Ord,
{
    let foreign = || {
        IndexError::new(
            IndexErrorKind::NetworkMismatch,
            format!(
                "address filter belongs to a network other than {}",
                network.chain
            ),
            false,
        )
    };
    let malformed = || {
        IndexError::new(
            IndexErrorKind::InvalidRequest,
            format!(
                "address filter is not a canonical {} address",
                network.chain
            ),
            false,
        )
    };
    addresses
        .iter()
        .map(|address| {
            if !address.belongs_to(network) {
                return Err(foreign());
            }
            A::try_from(address).map_err(|_| malformed())
        })
        .collect()
}

#[cfg(test)]
mod tests {

    use super::*;

    fn network(chain: &str, network: &str) -> Network {
        Network::new(ChainName::new(chain), network)
    }

    /// A native address double: canonical decimal text only.
    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct Number(u32);

    impl TryFrom<&Address> for Number {
        type Error = ();

        fn try_from(address: &Address) -> Result<Self, ()> {
            let number = address.value.parse::<u32>().map_err(|_| ())?;
            (number.to_string() == address.value)
                .then_some(Self(number))
                .ok_or(())
        }
    }

    #[test]
    fn required_chain_rejects_foreign_chain_and_blank_network() {
        let chain = ChainName::new("test");
        assert!(require_chain(&network("test", "mainnet"), &chain).is_ok());
        for rejected in [network("other", "mainnet"), network("test", " ")] {
            let error = require_chain(&rejected, &chain).expect_err("network must be rejected");
            assert_eq!(error.kind, IndexErrorKind::NetworkMismatch);
            assert!(!error.retryable);
        }
    }

    #[test]
    fn selected_addresses_parse_into_a_native_set() {
        let network = network("test", "mainnet");
        let addresses = [
            Address::new(network.clone(), "7"),
            Address::new(network.clone(), "3"),
            Address::new(network.clone(), "7"),
        ];

        let selected = select_addresses::<Number>(&network, &addresses).expect("valid selection");

        assert_eq!(
            selected.into_iter().collect::<Vec<_>>(),
            [Number(3), Number(7)]
        );
    }

    #[test]
    fn selected_addresses_reject_foreign_network_and_non_canonical_text() {
        let network = network("test", "mainnet");
        for (address, kind) in [
            (
                Address::new(self::network("test", "testnet"), "1"),
                IndexErrorKind::NetworkMismatch,
            ),
            (
                Address::new(network.clone(), "not-a-number"),
                IndexErrorKind::InvalidRequest,
            ),
            (
                Address::new(network.clone(), "07"),
                IndexErrorKind::InvalidRequest,
            ),
        ] {
            let error = select_addresses::<Number>(&network, &[address])
                .expect_err("selection must be rejected");
            assert_eq!(error.kind, kind);
            assert!(!error.retryable);
        }
    }
}
