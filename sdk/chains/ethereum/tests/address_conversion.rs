//! Native Ethereum addresses convert losslessly to and from `base::Address`,
//! and user-supplied text validates from a `Network` alone, with no store,
//! node, or index.

use base::Networked;
use chain_ethereum::{Address, ErrorKind, Network, WalletConfig};
use wallets::{AddressParser, WalletErrorKind};

#[test]
fn canonical_address_is_lowercase_hex_and_round_trips_within_ethereum() {
    let network = Network::new(31_337, "anvil").canonical();
    let address = Address::from_bytes(std::array::from_fn(|index| index as u8));
    let canonical: base::Address = address.canonical(&network);

    assert_eq!(canonical.network, network);
    assert_eq!(
        canonical.value,
        "0x000102030405060708090a0b0c0d0e0f10111213"
    );
    assert_eq!(Address::try_from(&canonical), Ok(address));
}

#[test]
fn strict_conversion_rejects_a_foreign_chain_and_non_canonical_text() {
    let network = Network::new(31_337, "anvil").canonical();
    let canonical = Address::from_bytes([0xab; 20]).canonical(&network);
    let foreign = base::Address::new(
        base::Network::new(base::ChainName::new("bitcoin"), "anvil"),
        &canonical.value,
    );
    let checksummed = base::Address::new(
        network.clone(),
        "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
    );
    let malformed = base::Address::new(network, "0xab");
    for address in [foreign, checksummed, malformed] {
        assert_eq!(
            Address::try_from(&address).unwrap_err().kind,
            ErrorKind::InvalidAddress
        );
    }
}

/// What a consumer embedding this crate over its own store can do: a
/// `Network`, a string, and nothing else. No `RpcClient`, no `Indexer`, and
/// therefore no `WalletFactory` — so no reason to fall back to
/// `base::Address::new` and trust itself.
///
/// The proof this parse offers is the weakest of the three chains: single-case
/// hex carries no checksum, so only mixed-case text is checked.
#[test]
fn user_text_validates_from_a_network_alone() {
    let network = Network::new(31_337, "anvil");
    let config = WalletConfig::new(&network);
    let canonical = "0x5aaeb6053f3e94c9b9a09f33669435e7ef1beaed";
    for text in [
        canonical,
        "0x5AAEB6053F3E94C9B9A09F33669435E7EF1BEAED",
        "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
    ] {
        let address = config
            .parse_address(text)
            .expect("any-case hex, EIP-55 checked when mixed");
        assert_eq!(address.value, canonical);
        assert_eq!(address.network, network.canonical());
    }
    // Refused: no prefix, the wrong length, and a mixed-case checksum failure.
    for text in [
        "",
        "5aaeb6053f3e94c9b9a09f33669435e7ef1beaed",
        "0xab",
        "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAeD",
    ] {
        assert_eq!(
            config.parse_address(text).unwrap_err().kind,
            WalletErrorKind::InvalidAddress
        );
    }
}
