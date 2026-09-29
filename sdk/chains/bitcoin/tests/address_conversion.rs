//! Native Bitcoin addresses convert losslessly to and from their canonical
//! (chain-neutral) form, and only within their own network — and user-supplied
//! text validates from a `Network` alone, with no store, node, or index.

use base::{ChainName, Networked};
use chain_bitcoin::{Address, AddressType, ErrorKind, KeyPair, Network, WalletConfig};
use wallets::{AddressParser, WalletErrorKind};

fn address(network: Network, kind: AddressType) -> Address {
    KeyPair::import(crypto::SecretBytes::new([1; 32]))
        .expect("valid scalar")
        .address(network, kind)
}

#[test]
fn canonical_address_round_trips_within_its_network_only() {
    for kind in [AddressType::SegwitV0, AddressType::Taproot] {
        let native = address(Network::Regtest, kind);
        let canonical: base::Address = native.canonical(&Network::Regtest.canonical());
        assert_eq!(canonical.value, native.to_string());
        assert_eq!(Address::try_from(&canonical), Ok(native.clone()));
        for network in [Network::Mainnet, Network::Signet] {
            let foreign = base::Address::new(network.canonical(), canonical.value.clone());
            let error = Address::try_from(&foreign).expect_err("another network must fail");
            assert_eq!(error.kind, ErrorKind::InvalidAddress);
        }
    }
}

#[test]
fn canonical_conversion_rejects_other_chains_and_non_canonical_text() {
    let native = address(Network::Regtest, AddressType::SegwitV0);
    let text = native.to_string();
    for (network, value) in [
        (
            base::Network::new(ChainName::new("ethereum"), "regtest"),
            text.clone(),
        ),
        (
            base::Network::new(chain_bitcoin::CHAIN_NAME, "main"),
            text.clone(),
        ),
        (Network::Regtest.canonical(), text.to_uppercase()),
    ] {
        let error = Address::try_from(&base::Address::new(network, value))
            .expect_err("only exact canonical text of a known network converts");
        assert_eq!(error.kind, ErrorKind::InvalidAddress);
    }
}

/// What a consumer embedding this crate over its own store can do: a
/// `Network`, a string, and nothing else. No `OutputStore`, no `RpcClient`, no
/// `Indexer`, and therefore no `WalletFactory` — so no reason to fall back to
/// `base::Address::new` and trust itself.
#[test]
fn user_text_validates_from_a_network_alone() {
    let config = WalletConfig::new(&Network::Regtest);
    let native = address(Network::Regtest, AddressType::Taproot);
    let canonical = native.canonical(&Network::Regtest.canonical());
    for text in [native.to_string(), native.to_string().to_uppercase()] {
        assert_eq!(
            config
                .parse_address(&text)
                .expect("any-case text of this network"),
            canonical
        );
    }
    // Refused: another network's prefix, and text that is no address at all.
    for text in [
        address(Network::Mainnet, AddressType::SegwitV0).to_string(),
        String::new(),
        "not-an-address".to_owned(),
    ] {
        assert_eq!(
            config.parse_address(&text).unwrap_err().kind,
            WalletErrorKind::InvalidAddress
        );
    }
}

#[test]
fn parsing_for_a_network_accepts_any_case_and_returns_canonical_text() {
    let native = address(Network::Regtest, AddressType::Taproot);
    let parsed = Address::parse(&native.to_string().to_uppercase(), Network::Regtest)
        .expect("Bech32m is case-insensitive");
    assert_eq!(parsed, native);
    assert!(Address::parse(native.as_str(), Network::Mainnet).is_err());
}
