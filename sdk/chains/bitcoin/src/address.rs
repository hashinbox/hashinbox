//! [`Address`]: a Bitcoin address of one network, usable on its own or as a
//! [`base::Address`].

use std::{fmt, str::FromStr as _};

use anyhow::Context as _;
use bitcoin::{
    CompressedPublicKey, Script, ScriptBuf, XOnlyPublicKey, address::NetworkUnchecked,
    secp256k1::Secp256k1,
};

use crate::Network;

/// Which address a key derives.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AddressType {
    /// P2WPKH, signed with ECDSA.
    #[default]
    SegwitV0,
    /// P2TR key path (no script tree), signed with Schnorr.
    Taproot,
}

/// A Bitcoin address of one network, with its canonical [`base::Address`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    native: bitcoin::Address,
    address: base::Address,
}

impl Address {
    /// The address `script` pays on `network`; `None` for a script with no
    /// address (`OP_RETURN`, bare multisig, other non-standard scripts).
    #[must_use]
    pub fn from_script(script: &Script, network: Network) -> Option<Self> {
        let native = bitcoin::Address::from_script(script, network.native()).ok()?;
        Some(Self::new(native, network))
    }

    /// The `kind` address of `public` on `network`.
    #[must_use]
    pub fn from_public_key(
        public: &CompressedPublicKey,
        kind: AddressType,
        network: Network,
    ) -> Self {
        let native = match kind {
            AddressType::SegwitV0 => bitcoin::Address::p2wpkh(public, network.native()),
            AddressType::Taproot => bitcoin::Address::p2tr(
                &Secp256k1::verification_only(),
                XOnlyPublicKey::from(public.0),
                None,
                network.native(),
            ),
        };
        Self::new(native, network)
    }

    fn new(native: bitcoin::Address, network: Network) -> Self {
        let address = base::Address::new(network.into(), native.to_string());
        Self { native, address }
    }

    /// The script that pays it.
    #[must_use]
    pub fn script_pubkey(&self) -> ScriptBuf {
        self.native.script_pubkey()
    }
}

impl base::Addressed for Address {
    fn address(&self) -> &base::Address {
        &self.address
    }
}

impl base::Networked for Address {
    fn network(&self) -> &base::Network {
        &self.address.network
    }
}

impl Address {
    /// `text`, in any case, as an address of `network`.
    ///
    /// # Errors
    ///
    /// For invalid text or an address of another network.
    pub fn parse(text: &str, network: Network) -> anyhow::Result<Self> {
        let native = bitcoin::Address::<NetworkUnchecked>::from_str(text)
            .context("invalid Bitcoin address")?
            .require_network(network.native())
            .context("Bitcoin address is for another network")?;
        Ok(Self::new(native, network))
    }

    /// The address the hex-encoded `script` pays on `network`.
    ///
    /// # Errors
    ///
    /// For invalid hex or a script with no address.
    pub fn from_script_hex(hex: &str, network: Network) -> anyhow::Result<Self> {
        let script = ScriptBuf::from_hex(hex).context("invalid Bitcoin script hex")?;
        Self::from_script(&script, network).context("Bitcoin script has no address")
    }

    /// The `bitcoin` crate's address.
    #[must_use]
    pub const fn as_native(&self) -> &bitcoin::Address {
        &self.native
    }
}

impl fmt::Display for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.native.fmt(formatter)
    }
}

impl From<Address> for base::Address {
    fn from(address: Address) -> Self {
        address.address
    }
}
