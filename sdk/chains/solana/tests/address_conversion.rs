//! Native Solana addresses convert losslessly to and from `base::Address`,
//! which converts back only from exact canonical text — and user-supplied text
//! validates from a `Network` alone, with no store, node, or index.

use base::{ChainName, Networked};
use chain_solana::{Address, ErrorKind, Network, WalletConfig};
use wallets::{AddressParser, WalletErrorKind};

fn network() -> Network {
    Network::new(
        "localnet",
        "11111111111111111111111111111111"
            .parse()
            .expect("canonical genesis"),
    )
}

#[test]
fn canonical_address_round_trips_within_solana_only() {
    let network = network().canonical();
    let bytes = std::array::from_fn(|index| index as u8);
    let address = Address::from_bytes(bytes);
    let canonical: base::Address = address.canonical(&network);
    assert_eq!(canonical.value, address.to_string());
    assert_eq!(Address::try_from(&canonical).unwrap().as_bytes(), &bytes);
    assert_eq!(Address::try_from(&canonical), Ok(address.clone()));
    for (network, value) in [
        (
            base::Network::new(ChainName::new("bitcoin"), "localnet"),
            canonical.value.clone(),
        ),
        (network.clone(), format!(" {}", canonical.value)),
        (network, "not-base58".to_owned()),
    ] {
        let error = Address::try_from(&base::Address::new(network, value))
            .expect_err("only canonical Solana text converts");
        assert_eq!(error.kind, ErrorKind::InvalidAddress);
    }
}

/// What a consumer embedding this crate over its own store can do: a
/// `Network`, a string, and nothing else. No `ConfirmedHistory`, no `Indexer`,
/// and therefore no `WalletFactory` — so no reason to fall back to
/// `base::Address::new` and trust itself.
///
/// It is also where this parse stops: canonical Base58 of thirty-two bytes is
/// all it proves. An off-curve (program-derived) address is a valid identity
/// and parses; whether native SOL may be sent to one is a transfer rule.
#[test]
fn user_text_validates_from_a_network_alone() {
    let config = WalletConfig::new(&network());
    let address = Address::from_bytes(std::array::from_fn(|index| index as u8));
    assert_eq!(
        config
            .parse_address(&address.to_string())
            .expect("canonical Base58 of 32 bytes"),
        address.canonical(&network().canonical())
    );
    let off_curve = (0..=u8::MAX)
        .map(|byte| Address::from_bytes([byte; 32]))
        .find(|address| !address.is_on_curve())
        .expect("some repeated byte is off the Ed25519 curve");
    assert!(config.parse_address(&off_curve.to_string()).is_ok());
    // Refused: empty, padded, hex-prefixed, and too few bytes.
    for text in [
        String::new(),
        format!("{address} "),
        format!("0x{address}"),
        "0".to_owned(),
    ] {
        assert_eq!(
            config.parse_address(&text).unwrap_err().kind,
            WalletErrorKind::InvalidAddress
        );
    }
}
