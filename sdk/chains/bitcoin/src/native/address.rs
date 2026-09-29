use std::{fmt, str::FromStr};

use bitcoin::{Address as NativeAddress, Script, ScriptBuf, address::NetworkUnchecked};

use super::{Error, ErrorKind, Network};

/// A Bitcoin address in its canonical text form (lowercase for Bech32).
///
/// The text alone does not name a network for every address kind, so
/// operations that need one ([`Address::parse`], [`Address::script_pubkey`])
/// take it explicitly.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Address(String);

/// Which address a key derives: the two script kinds this crate can spend.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum AddressType {
    /// P2WPKH (Bech32).
    #[default]
    SegwitV0,
    /// P2TR key-path (Bech32m).
    Taproot,
}

impl Address {
    /// Parses `text` for `network` and returns its canonical form. Accepts
    /// any case Bitcoin allows, e.g. uppercase Bech32.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidAddress`] for malformed text or another network.
    pub fn parse(text: &str, network: Network) -> Result<Self, Error> {
        Self::checked(text, network).map(|address| Self(address.to_string()))
    }

    /// The address that `script` pays on `network`, if it has one.
    #[must_use]
    pub fn from_script(script: &Script, network: Network) -> Option<Self> {
        NativeAddress::from_script(script, network.native())
            .ok()
            .map(|address| Self(address.to_string()))
    }

    /// The locking script of this address on `network`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidAddress`] when the text is invalid for `network`.
    pub fn script_pubkey(&self, network: Network) -> Result<ScriptBuf, Error> {
        Self::checked(&self.0, network).map(|address| address.script_pubkey())
    }

    /// The standard script kind this address pays on `network`, if any.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidAddress`] when the text is invalid for `network`.
    pub fn kind(&self, network: Network) -> Result<Option<bitcoin::AddressType>, Error> {
        Self::checked(&self.0, network).map(|address| address.address_type())
    }

    /// The canonical text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Wraps text without validation, for evidence that is checked later.
    pub(crate) fn unchecked(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    fn checked(text: &str, network: Network) -> Result<NativeAddress, Error> {
        text.parse::<NativeAddress<NetworkUnchecked>>()
            .map_err(|error| Self::invalid(format!("invalid Bitcoin address: {error}")))?
            .require_network(network.native())
            .map_err(|error| {
                Self::invalid(format!("Bitcoin address is for the wrong network: {error}"))
            })
    }

    fn invalid(message: String) -> Error {
        Error::new(ErrorKind::InvalidAddress, message)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Canonical text of any network; see [`Address::parse`] to check one network
/// and to accept non-canonical case.
impl FromStr for Address {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let address = text
            .parse::<NativeAddress<NetworkUnchecked>>()
            .map_err(|error| Self::invalid(format!("invalid Bitcoin address: {error}")))?
            .assume_checked()
            .to_string();
        if address != text {
            return Err(Self::invalid(
                "Bitcoin address is not in canonical form".to_owned(),
            ));
        }
        Ok(Self(address))
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::{CompressedPublicKey, PublicKey};

    use super::*;

    fn address(network: Network) -> String {
        let public_key = PublicKey::from_slice(&[
            0x02, 0x79, 0xbe, 0x66, 0x7e, 0xf9, 0xdc, 0xbb, 0xac, 0x55, 0xa0, 0x62, 0x95, 0xce,
            0x87, 0x0b, 0x07, 0x02, 0x9b, 0xfc, 0xdb, 0x2d, 0xce, 0x28, 0xd9, 0x59, 0xf2, 0x81,
            0x5b, 0x16, 0xf8, 0x17, 0x98,
        ])
        .expect("test public key must parse");
        NativeAddress::p2wpkh(
            &CompressedPublicKey::try_from(public_key).expect("test public key must be compressed"),
            network.native(),
        )
        .to_string()
    }

    #[test]
    fn parser_checks_network_and_returns_canonical_display() {
        let regtest = address(Network::Regtest);
        let parsed = Address::parse(&regtest.to_uppercase(), Network::Regtest)
            .expect("regtest address must parse in any case");
        assert_eq!(parsed.as_str(), regtest);
        assert_eq!(
            Address::parse(&regtest, Network::Mainnet).unwrap_err().kind,
            ErrorKind::InvalidAddress
        );
        assert!(parsed.script_pubkey(Network::Regtest).unwrap().is_p2wpkh());
    }

    #[test]
    fn from_str_accepts_any_network_but_only_canonical_text() {
        for network in [Network::Mainnet, Network::Regtest] {
            let text = address(network);
            assert_eq!(text.parse::<Address>().unwrap().to_string(), text);
            let error = text.to_uppercase().parse::<Address>().unwrap_err();
            assert_eq!(error.message, "Bitcoin address is not in canonical form");
        }
        assert!("not-a-bitcoin-address".parse::<Address>().is_err());
    }

    #[test]
    fn script_constructor_preserves_legacy_address_encodings() {
        for (script, expected) in [
            (
                "76a914162c5ea71c0b23f5b9022ef047c4a86470a5b07088ac",
                "132F25rTsvBdp9JzLLBHP5mvGY66i1xdiM",
            ),
            (
                "a914162c5ea71c0b23f5b9022ef047c4a86470a5b07087",
                "33iFwdLuRpW1uK1RTRqsoi8rR4NpDzk66k",
            ),
        ] {
            let script = ScriptBuf::from_hex(script).expect("fixture script must decode");
            let converted = Address::from_script(&script, Network::Mainnet)
                .expect("legacy payment script must have an address");
            assert_eq!(converted.as_str(), expected);
        }
    }

    #[test]
    fn script_constructor_uses_requested_witness_network() {
        let script = Address::parse(&address(Network::Mainnet), Network::Mainnet)
            .unwrap()
            .script_pubkey(Network::Mainnet)
            .unwrap();
        for network in [
            Network::Mainnet,
            Network::Testnet3,
            Network::Testnet4,
            Network::Signet,
            Network::Regtest,
        ] {
            let converted = Address::from_script(&script, network).unwrap();
            assert_eq!(converted.as_str(), address(network));
            assert_eq!(converted.script_pubkey(network).unwrap(), script);
        }
    }

    #[test]
    fn script_constructor_preserves_future_witness_programs() {
        // Witness version 13 with a 40-byte program has an address even though
        // the SDK has no corresponding spending implementation.
        let script = ScriptBuf::from_bytes([vec![0x5d, 0x28], vec![0x11; 40]].concat());
        let converted = Address::from_script(&script, Network::Regtest).unwrap();
        assert!(converted.as_str().starts_with("bcrt1"));
        assert_eq!(converted.script_pubkey(Network::Regtest).unwrap(), script);
    }

    #[test]
    fn script_constructor_returns_none_when_no_address_exists() {
        for bytes in [
            vec![],
            vec![0x6a, 0x01, 0x01],
            vec![0x00, 0x01, 0x2a],
            vec![0x51],
        ] {
            let script = ScriptBuf::from_bytes(bytes);
            assert_eq!(Address::from_script(&script, Network::Regtest), None);
        }
    }
}
