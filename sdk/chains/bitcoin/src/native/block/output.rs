use bitcoin::{ScriptBuf, TxOut, hex::FromHex};
use serde_json::Value;

use crate::native::{Address, Error, Network, Satoshi, TransactionId, json::Object};

/// Witness weight of a P2WPKH key-path spend: signature and public key.
const P2WPKH_SATISFACTION_WEIGHT: u64 = 109;
/// Witness weight of a P2TR key-path spend: one Schnorr signature.
const P2TR_SATISFACTION_WEIGHT: u64 = 67;

/// Confirmations a coinbase output needs before it can be spent.
pub const COINBASE_MATURITY: u64 = 100;

/// A transaction output's identity: the creating transaction and its index.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Outpoint {
    /// The transaction that created the output.
    pub transaction_id: TransactionId,
    /// The output's position in that transaction.
    pub output_index: u32,
}

/// One transaction output: value, locking script, and the address it pays.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Output {
    /// Satoshis locked.
    pub value: Satoshi,
    /// The locking script. For a previous output resolved from compact
    /// evidence it is rebuilt from `address`, and empty when there is none.
    pub script_pubkey: Vec<u8>,
    /// The address `script_pubkey` pays, when it has a standard one.
    pub address: Option<Address>,
}

/// An unspent output a wallet can fund a transfer with.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Utxo {
    /// Which output.
    pub outpoint: Outpoint,
    /// Its value and locking script.
    pub output: Output,
}

impl Output {
    /// The output `native` pays on `network`.
    pub(crate) fn from_native(native: &TxOut, network: Network) -> Self {
        Self {
            value: Satoshi::from_atomic(native.value.to_sat()),
            script_pubkey: native.script_pubkey.to_bytes(),
            address: Address::from_script(&native.script_pubkey, network),
        }
    }

    /// The output that `address` would hold: its script is rebuilt, never read.
    pub(crate) fn resolved(value: Satoshi, address: Option<Address>, network: Network) -> Self {
        let script_pubkey = address
            .as_ref()
            .and_then(|address| address.script_pubkey(network).ok())
            .map(ScriptBuf::into_bytes)
            .unwrap_or_default();
        Self {
            value,
            script_pubkey,
            address,
        }
    }

    /// The locking script as a script.
    #[must_use]
    pub fn script(&self) -> ScriptBuf {
        ScriptBuf::from_bytes(self.script_pubkey.clone())
    }

    /// Checks the `vout` JSON claim at `position` against the consensus output.
    pub(super) fn parse(value: &Value, native: &TxOut, position: usize) -> Result<(), Error> {
        let object = Object::new(value, "Bitcoin transaction output", Error::invalid_block)?;
        let index = object.u64("n", "Bitcoin output index")?;
        if usize::try_from(index).ok() != Some(position) {
            return Err(object.fail("Bitcoin transaction output index does not match its position"));
        }
        let claimed = object
            .get("value")
            .ok_or_else(|| object.fail("Bitcoin output value is missing"))?;
        let claimed =
            Satoshi::from_btc(claimed, "Bitcoin output value").map_err(Error::invalid_block)?;
        if native.value.to_sat() != claimed.units() {
            return Err(object.fail("Bitcoin output value does not match its consensus bytes"));
        }
        let script = Self::script_hex(&object, "Bitcoin output scriptPubKey is missing")?;
        if native.script_pubkey != script {
            return Err(object.fail("Bitcoin output script does not match its consensus bytes"));
        }
        Ok(())
    }

    /// Reads an input's `prevout`: compact (`value_satoshis`, `address`) as
    /// the source writes it, or Bitcoin Core's verbosity-3 shape.
    pub(super) fn parse_previous(
        prevout: &Object<'_>,
        spending_height: u64,
        network: Network,
    ) -> Result<Self, Error> {
        let Some(value) = prevout.get("value_satoshis") else {
            return Self::parse_verbose(prevout, spending_height, network);
        };
        let value = value
            .as_u64()
            .ok_or_else(|| prevout.fail("Bitcoin compact prevout value is invalid"))?;
        let address = match prevout.get("address") {
            None => return Err(prevout.fail("Bitcoin compact prevout address fact is missing")),
            Some(Value::Null) => None,
            Some(Value::String(text)) => Some(Self::compact_address(prevout, text, network)?),
            Some(_) => return Err(prevout.fail("Bitcoin compact prevout address fact is invalid")),
        };
        Ok(Self::resolved(
            Satoshi::from_atomic(value),
            address,
            network,
        ))
    }

    fn compact_address(
        prevout: &Object<'_>,
        text: &str,
        network: Network,
    ) -> Result<Address, Error> {
        let address = Address::parse(text, network).map_err(|_| {
            prevout.fail("Bitcoin compact prevout address is invalid or wrong-network")
        })?;
        if address.as_str() != text {
            return Err(prevout.fail("Bitcoin compact prevout address is not canonical"));
        }
        Ok(address)
    }

    fn parse_verbose(
        prevout: &Object<'_>,
        spending_height: u64,
        network: Network,
    ) -> Result<Self, Error> {
        let value = prevout
            .get("value")
            .ok_or_else(|| prevout.fail("Bitcoin prevout value is missing"))?;
        let value =
            Satoshi::from_btc(value, "Bitcoin prevout value").map_err(Error::invalid_block)?;
        let script = Self::script_hex(prevout, "Bitcoin prevout scriptPubKey is missing")?;
        if prevout.u64("height", "Bitcoin prevout creation height")? > spending_height {
            return Err(
                prevout.fail("Bitcoin input previous output was created after the spending block")
            );
        }
        prevout.bool("generated", "Bitcoin prevout coinbase flag")?;
        Ok(Self {
            value,
            address: Address::from_script(&script, network),
            script_pubkey: script.into_bytes(),
        })
    }

    fn script_hex(object: &Object<'_>, missing: &str) -> Result<ScriptBuf, Error> {
        let script = object
            .get("scriptPubKey")
            .and_then(Value::as_object)
            .ok_or_else(|| object.fail(missing))?;
        let hex = script
            .get("hex")
            .and_then(Value::as_str)
            .ok_or_else(|| object.fail("Bitcoin scriptPubKey hex is missing or invalid"))?;
        Vec::<u8>::from_hex(hex)
            .map(ScriptBuf::from_bytes)
            .map_err(|_| object.fail("Bitcoin scriptPubKey hex is invalid"))
    }
}

impl Utxo {
    /// Witness weight of a key-path spend; `None` for scripts this crate cannot spend.
    #[must_use]
    pub fn satisfaction_weight(&self) -> Option<u64> {
        let script = self.output.script();
        if script.is_p2wpkh() {
            Some(P2WPKH_SATISFACTION_WEIGHT)
        } else if script.is_p2tr() {
            Some(P2TR_SATISFACTION_WEIGHT)
        } else {
            None
        }
    }
}
