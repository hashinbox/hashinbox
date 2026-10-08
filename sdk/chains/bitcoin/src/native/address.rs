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
