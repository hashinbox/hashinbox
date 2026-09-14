use std::{collections::BTreeMap, fmt};

use bitcoin::{ScriptBuf, Transaction as NativeTransaction, Txid, consensus, hex::FromHex};
use indexing::{BlockHeight, BlockRef};
use serde_json::{Map, Value};

use crate::{Network, Satoshi, TransactionId};

use super::Outpoint;

mod block;
mod value;

use value::{required_bool, required_string, required_u32, required_u64};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BlockData {
    pub reference: BlockRef,
    pub transactions: Vec<Transaction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Transaction {
    pub id: TransactionId,
    pub inputs: Vec<Input>,
    pub outputs: Vec<Output>,
    pub coinbase: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Input {
    pub previous_output: Option<PreviousOutput>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PreviousOutput {
    pub outpoint: Outpoint,
    pub value: Satoshi,
    pub address: Option<crate::Address>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Output {
    pub value: Satoshi,
    pub script_pubkey: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ParseError {
    message: String,
}

impl ParseError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Transaction {
    fn parse(
        value: &Value,
        block_height: BlockHeight,
        network: Network,
        same_block_outputs: &BTreeMap<Outpoint, PreviousOutput>,
    ) -> Result<Self, ParseError> {
        let object = value
            .as_object()
            .ok_or_else(|| ParseError::new("Bitcoin transaction must be an object"))?;
        let txid = required_string(object, "txid", "Bitcoin transaction ID")?
            .parse::<TransactionId>()
            .map_err(|_| ParseError::new("Bitcoin transaction ID is invalid"))?;
        let raw = Vec::<u8>::from_hex(required_string(object, "hex", "Bitcoin raw transaction")?)
            .map_err(|_| ParseError::new("Bitcoin transaction hex is invalid"))?;
        let native: NativeTransaction = consensus::deserialize(&raw)
            .map_err(|_| ParseError::new("Bitcoin transaction consensus bytes are invalid"))?;
        if TransactionId::from(native.compute_txid()) != txid {
            return Err(ParseError::new(
                "Bitcoin transaction ID does not match its consensus bytes",
            ));
        }
        Self::parse_verbose(
            object,
            txid,
            &native,
            block_height,
            network,
            same_block_outputs,
        )
    }

    fn parse_verbose(
        object: &Map<String, Value>,
        id: TransactionId,
        native: &NativeTransaction,
        block_height: BlockHeight,
        network: Network,
        same_block_outputs: &BTreeMap<Outpoint, PreviousOutput>,
    ) -> Result<Self, ParseError> {
        let coinbase = native.is_coinbase();
        let input_values = object
            .get("vin")
            .and_then(Value::as_array)
            .ok_or_else(|| ParseError::new("Bitcoin transaction inputs must be an array"))?;
        if input_values.len() != native.input.len() {
            return Err(ParseError::new(
                "Bitcoin transaction input count does not match its consensus bytes",
            ));
        }
        let mut inputs = Vec::with_capacity(input_values.len());
        for (index, (input, native_input)) in input_values.iter().zip(&native.input).enumerate() {
            inputs.push(Input::parse(
                input,
                native_input,
                index,
                coinbase,
                block_height,
                network,
                same_block_outputs,
            )?);
        }
        let output_values = object
            .get("vout")
            .and_then(Value::as_array)
            .ok_or_else(|| ParseError::new("Bitcoin transaction outputs must be an array"))?;
        if output_values.len() != native.output.len() {
            return Err(ParseError::new(
                "Bitcoin transaction output count does not match its consensus bytes",
            ));
        }
        let mut outputs = Vec::with_capacity(output_values.len());
        for (index, (output, expected)) in output_values.iter().zip(&native.output).enumerate() {
            outputs.push(Output::parse(output, expected, index)?);
        }
        Ok(Self {
            id,
            inputs,
            outputs,
            coinbase,
        })
    }
}

impl Output {
    fn parse(output: &Value, native: &bitcoin::TxOut, position: usize) -> Result<Self, ParseError> {
        let object = output
            .as_object()
            .ok_or_else(|| ParseError::new("Bitcoin transaction output must be an object"))?;
        let output_index = required_u64(object, "n", "Bitcoin output index")?;
        if usize::try_from(output_index).ok() != Some(position) {
            return Err(ParseError::new(
                "Bitcoin transaction output index does not match its position",
            ));
        }
        let value = Satoshi::from_block_json(
            object
                .get("value")
                .ok_or_else(|| ParseError::new("Bitcoin output value is missing"))?,
            "Bitcoin output value",
        )?;
        if native.value.to_sat() != value.0 {
            return Err(ParseError::new(
                "Bitcoin output value does not match its consensus bytes",
            ));
        }
        let script = object
            .get("scriptPubKey")
            .and_then(Value::as_object)
            .ok_or_else(|| ParseError::new("Bitcoin output scriptPubKey is missing"))?;
        let hex = script
            .get("hex")
            .and_then(Value::as_str)
            .ok_or_else(|| ParseError::new("Bitcoin scriptPubKey hex is missing or invalid"))?;
        let bytes = Vec::<u8>::from_hex(hex)
            .map_err(|_| ParseError::new("Bitcoin scriptPubKey hex is invalid"))?;
        let script = ScriptBuf::from_bytes(bytes);
        if native.script_pubkey != script {
            return Err(ParseError::new(
                "Bitcoin output script does not match its consensus bytes",
            ));
        }
        Ok(Self {
            value,
            script_pubkey: script.into_bytes(),
        })
    }
}

impl Input {
    fn parse(
        input: &Value,
        native_input: &bitcoin::TxIn,
        index: usize,
        coinbase: bool,
        block_height: BlockHeight,
        network: Network,
        same_block_outputs: &BTreeMap<Outpoint, PreviousOutput>,
    ) -> Result<Self, ParseError> {
        let object = input
            .as_object()
            .ok_or_else(|| ParseError::new("Bitcoin transaction input must be an object"))?;
        if native_input.previous_output.is_null() {
            if !coinbase || object.get("coinbase").and_then(Value::as_str).is_none() {
                return Err(ParseError::new(
                    "Bitcoin null input is not a valid coinbase input",
                ));
            }
            return Ok(Self {
                previous_output: None,
            });
        }
        if coinbase {
            return Err(ParseError::new(
                "Bitcoin coinbase transaction contains a non-coinbase input",
            ));
        }
        let outpoint = Outpoint::parse_input(object, &native_input.previous_output, index)?;
        let previous_output = PreviousOutput::resolve(
            object,
            outpoint,
            index,
            block_height,
            network,
            same_block_outputs,
        )?;
        Ok(Self {
            previous_output: Some(previous_output),
        })
    }
}

impl Outpoint {
    fn parse_input(
        object: &Map<String, Value>,
        native: &bitcoin::OutPoint,
        index: usize,
    ) -> Result<Self, ParseError> {
        let previous_id = required_string(object, "txid", "Bitcoin input previous transaction ID")?
            .parse::<TransactionId>()
            .map_err(|_| ParseError::new("Bitcoin transaction ID is invalid"))?;
        let output_index = required_u32(object, "vout", "Bitcoin input output index")?;
        if native.txid != Txid::from(previous_id) || native.vout != output_index {
            return Err(ParseError::new(format!(
                "Bitcoin input {index} outpoint does not match its consensus bytes"
            )));
        }
        Ok(Self {
            transaction_id: previous_id,
            output_index,
        })
    }
}

impl PreviousOutput {
    fn from_output(
        transaction_id: TransactionId,
        index: usize,
        output: &Output,
        network: Network,
    ) -> Result<Self, ParseError> {
        let output_index = u32::try_from(index)
            .map_err(|_| ParseError::new("Bitcoin output index exceeds u32"))?;
        let script = ScriptBuf::from_bytes(output.script_pubkey.clone());
        Ok(Self {
            outpoint: Outpoint {
                transaction_id,
                output_index,
            },
            value: output.value,
            address: crate::Address::from_script_for_network(&script, network),
        })
    }

    fn resolve(
        object: &Map<String, Value>,
        outpoint: Outpoint,
        index: usize,
        block_height: BlockHeight,
        network: Network,
        same_block_outputs: &BTreeMap<Outpoint, PreviousOutput>,
    ) -> Result<Self, ParseError> {
        let local = same_block_outputs.get(&outpoint);
        match object.get("prevout").and_then(Value::as_object) {
            Some(prevout) => {
                let resolved = PreviousOutput::parse(prevout, outpoint, block_height, network)?;
                let conflicts = local.is_some_and(|local| local != &resolved);
                if conflicts {
                    return Err(ParseError::new(format!(
                        "Bitcoin input {index} prevout conflicts with an earlier same-block output"
                    )));
                }
                Ok(resolved)
            }
            None => local.cloned().ok_or_else(|| {
                ParseError::new(format!(
                    "Bitcoin input {index} has no resolved previous output"
                ))
            }),
        }
    }

    fn parse(
        prevout: &Map<String, Value>,
        outpoint: Outpoint,
        spending_height: BlockHeight,
        network: Network,
    ) -> Result<Self, ParseError> {
        let Some(value) = prevout.get("value_satoshis") else {
            return Self::parse_verbose(prevout, outpoint, spending_height, network);
        };
        let value = value
            .as_u64()
            .ok_or_else(|| ParseError::new("Bitcoin compact prevout value is invalid"))?;
        let address = match prevout
            .get("address")
            .ok_or_else(|| ParseError::new("Bitcoin compact prevout address fact is missing"))?
        {
            Value::Null => None,
            Value::String(address) => {
                let parsed = crate::Address::parse_for_network(address, network);
                let canonical = parsed.map_err(|_| {
                    ParseError::new("Bitcoin compact prevout address is invalid or wrong-network")
                })?;
                if canonical.encoded() != address {
                    return Err(ParseError::new(
                        "Bitcoin compact prevout address is not canonical",
                    ));
                }
                Some(canonical)
            }
            _ => {
                return Err(ParseError::new(
                    "Bitcoin compact prevout address fact is invalid",
                ));
            }
        };
        Ok(Self {
            outpoint,
            value: Satoshi(value),
            address,
        })
    }

    fn parse_verbose(
        prevout: &Map<String, Value>,
        outpoint: Outpoint,
        spending_height: BlockHeight,
        network: Network,
    ) -> Result<Self, ParseError> {
        // Direct Block::parse callers may supply Bitcoin Core's
        // verbosity-3-compatible previous-output shape.
        let value = Satoshi::from_block_json(
            prevout
                .get("value")
                .ok_or_else(|| ParseError::new("Bitcoin prevout value is missing"))?,
            "Bitcoin prevout value",
        )?;
        let script = prevout
            .get("scriptPubKey")
            .and_then(Value::as_object)
            .ok_or_else(|| ParseError::new("Bitcoin prevout scriptPubKey is missing"))?;
        let hex = script
            .get("hex")
            .and_then(Value::as_str)
            .ok_or_else(|| ParseError::new("Bitcoin scriptPubKey hex is missing or invalid"))?;
        let bytes = Vec::<u8>::from_hex(hex)
            .map_err(|_| ParseError::new("Bitcoin scriptPubKey hex is invalid"))?;
        let script = ScriptBuf::from_bytes(bytes);
        let created_height = BlockHeight(required_u64(
            prevout,
            "height",
            "Bitcoin prevout creation height",
        )?);
        if created_height > spending_height {
            return Err(ParseError::new(
                "Bitcoin input previous output was created after the spending block",
            ));
        }
        let _coinbase = required_bool(prevout, "generated", "Bitcoin prevout coinbase flag")?;
        Ok(Self {
            outpoint,
            value,
            address: crate::Address::from_script_for_network(&script, network),
        })
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::{hashes::Hash, hex::DisplayHex};

    use super::*;

    #[test]
    fn inputs_preserve_coinbase_rules_before_outpoint_and_prevout_validation() {
        let null = bitcoin::OutPoint::null();
        let regular = bitcoin::OutPoint::new(Txid::from_byte_array([1; 32]), 0);
        for (outpoint, coinbase, input, expected) in [
            (
                null,
                true,
                Value::Null,
                Some("Bitcoin transaction input must be an object"),
            ),
            (null, true, serde_json::json!({"coinbase": ""}), None),
            (
                null,
                true,
                serde_json::json!({"coinbase": null}),
                Some("Bitcoin null input is not a valid coinbase input"),
            ),
            (
                null,
                false,
                serde_json::json!({"coinbase": "00"}),
                Some("Bitcoin null input is not a valid coinbase input"),
            ),
            (
                regular,
                true,
                serde_json::json!({"txid": "invalid", "prevout": {}}),
                Some("Bitcoin coinbase transaction contains a non-coinbase input"),
            ),
        ] {
            let native = bitcoin::TxIn {
                previous_output: outpoint,
                script_sig: ScriptBuf::new(),
                sequence: bitcoin::Sequence::MAX,
                witness: bitcoin::Witness::new(),
            };
            let result = Input::parse(
                &input,
                &native,
                0,
                coinbase,
                BlockHeight(10),
                Network::Regtest,
                &BTreeMap::new(),
            );
            match expected {
                Some(message) => assert_eq!(result.unwrap_err().to_string(), message),
                None => assert_eq!(result.unwrap().previous_output, None),
            }
        }
    }

    #[test]
    fn transaction_projection_preserves_consensus_checks_and_error_precedence() {
        let native = NativeTransaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![bitcoin::TxIn {
                previous_output: bitcoin::OutPoint::null(),
                script_sig: ScriptBuf::new(),
                sequence: bitcoin::Sequence::MAX,
                witness: bitcoin::Witness::new(),
            }],
            output: vec![
                bitcoin::TxOut {
                    value: bitcoin::Amount::from_sat(10),
                    script_pubkey: ScriptBuf::from_bytes(vec![0x6a]),
                },
                bitcoin::TxOut {
                    value: bitcoin::Amount::from_sat(20),
                    script_pubkey: ScriptBuf::new(),
                },
            ],
        };
        let original = serde_json::json!({
            "txid": native.compute_txid().to_string(),
            "hex": consensus::serialize(&native).to_lower_hex_string(),
            "vin": [{"coinbase": "00"}],
            "vout": [
                {
                    "n": 0,
                    "value": "0.00000010".parse::<serde_json::Number>().unwrap(),
                    "scriptPubKey": {"hex": "6a"},
                },
                {
                    "n": 1,
                    "value": "0.00000020".parse::<serde_json::Number>().unwrap(),
                    "scriptPubKey": {"hex": ""},
                },
            ],
        });
        let parsed = Transaction::parse(
            &original,
            BlockHeight(10),
            Network::Regtest,
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(parsed.id, TransactionId::from(native.compute_txid()));
        assert!(parsed.coinbase);
        assert_eq!(
            parsed.inputs,
            vec![Input {
                previous_output: None
            }]
        );
        assert_eq!(
            parsed.outputs,
            vec![
                Output {
                    value: Satoshi(10),
                    script_pubkey: vec![0x6a]
                },
                Output {
                    value: Satoshi(20),
                    script_pubkey: vec![]
                },
            ]
        );

        for (path, invalid, message) in [
            (
                "/txid",
                serde_json::json!("00".repeat(32)),
                "Bitcoin transaction ID does not match its consensus bytes",
            ),
            (
                "/hex",
                serde_json::json!("ff"),
                "Bitcoin transaction consensus bytes are invalid",
            ),
            (
                "/vin",
                Value::Null,
                "Bitcoin transaction inputs must be an array",
            ),
            (
                "/vin",
                serde_json::json!([]),
                "Bitcoin transaction input count does not match its consensus bytes",
            ),
            (
                "/vin/0",
                Value::Null,
                "Bitcoin transaction input must be an object",
            ),
            (
                "/vout",
                Value::Null,
                "Bitcoin transaction outputs must be an array",
            ),
            (
                "/vout",
                serde_json::json!([]),
                "Bitcoin transaction output count does not match its consensus bytes",
            ),
            (
                "/vout/0",
                Value::Null,
                "Bitcoin transaction output must be an object",
            ),
            (
                "/vout/0/n",
                serde_json::json!(1),
                "Bitcoin transaction output index does not match its position",
            ),
            (
                "/vout/0/value",
                serde_json::json!(0),
                "Bitcoin output value does not match its consensus bytes",
            ),
            (
                "/vout/0/scriptPubKey",
                Value::Null,
                "Bitcoin output scriptPubKey is missing",
            ),
            (
                "/vout/0/scriptPubKey/hex",
                serde_json::json!("6b"),
                "Bitcoin output script does not match its consensus bytes",
            ),
        ] {
            let mut transaction = original.clone();
            // Earlier transaction/input/output errors must win over the later output.
            transaction["vout"][1]["value"] = serde_json::json!(0);
            *transaction.pointer_mut(path).unwrap() = invalid;
            let error = Transaction::parse(
                &transaction,
                BlockHeight(10),
                Network::Regtest,
                &BTreeMap::new(),
            )
            .unwrap_err();
            assert_eq!(error.to_string(), message, "{path}");
        }

        let mut transaction = original;
        transaction["vout"][0]["n"] = serde_json::json!(1);
        transaction["vout"][0]["value"] = serde_json::json!(0);
        transaction["vout"][0]["scriptPubKey"] = Value::Null;
        for (field, restored, message) in [
            (
                "n",
                serde_json::json!(0),
                "Bitcoin transaction output index does not match its position",
            ),
            (
                "value",
                serde_json::json!("0.00000010".parse::<serde_json::Number>().unwrap()),
                "Bitcoin output value does not match its consensus bytes",
            ),
        ] {
            let error = Transaction::parse(
                &transaction,
                BlockHeight(10),
                Network::Regtest,
                &BTreeMap::new(),
            )
            .unwrap_err();
            assert_eq!(error.to_string(), message);
            transaction["vout"][0][field] = restored;
        }
    }

    #[test]
    fn prevout_formats_preserve_compact_priority_and_validation() {
        let outpoint = Outpoint {
            transaction_id: TransactionId([3; 32]),
            output_index: 2,
        };
        let script = ScriptBuf::from_bytes([vec![0, 20], vec![7; 20]].concat());
        let address = crate::Address::from_script_for_network(&script, Network::Regtest).unwrap();
        let wrong_network =
            crate::Address::from_script_for_network(&script, Network::Mainnet).unwrap();
        let verbose = serde_json::json!({
            "value": "0.00000010".parse::<serde_json::Number>().unwrap(),
            "scriptPubKey": { "hex": script.as_bytes().to_lower_hex_string() },
            "height": 10, "generated": false,
        });
        let parsed = PreviousOutput::parse(
            verbose.as_object().unwrap(),
            outpoint,
            BlockHeight(10),
            Network::Regtest,
        )
        .unwrap();
        assert_eq!(
            parsed,
            PreviousOutput {
                outpoint,
                value: Satoshi(10),
                address: Some(address.clone()),
            }
        );

        let mut compact = verbose.clone();
        compact["value_satoshis"] = serde_json::json!(u64::MAX);
        compact["address"] = serde_json::json!(address.encoded());
        compact["height"] = Value::Null;
        assert_eq!(
            PreviousOutput::parse(
                compact.as_object().unwrap(),
                outpoint,
                BlockHeight(10),
                Network::Regtest,
            )
            .unwrap(),
            PreviousOutput {
                outpoint,
                value: Satoshi(u64::MAX),
                address: Some(address.clone()),
            }
        );

        for (field, value, message) in [
            (
                "value_satoshis",
                Value::Null,
                "Bitcoin compact prevout value is invalid",
            ),
            (
                "address",
                serde_json::json!(false),
                "Bitcoin compact prevout address fact is invalid",
            ),
            (
                "address",
                serde_json::json!(wrong_network.encoded()),
                "Bitcoin compact prevout address is invalid or wrong-network",
            ),
            (
                "address",
                serde_json::json!(address.encoded().to_uppercase()),
                "Bitcoin compact prevout address is not canonical",
            ),
        ] {
            let mut invalid = compact.clone();
            invalid[field] = value;
            let error = PreviousOutput::parse(
                invalid.as_object().unwrap(),
                outpoint,
                BlockHeight(10),
                Network::Regtest,
            )
            .unwrap_err();
            assert_eq!(error.to_string(), message);
        }

        let mut invalid = verbose;
        invalid["height"] = serde_json::json!(11);
        invalid["generated"] = Value::Null;
        let error = PreviousOutput::parse(
            invalid.as_object().unwrap(),
            outpoint,
            BlockHeight(10),
            Network::Regtest,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Bitcoin input previous output was created after the spending block"
        );
    }

    #[test]
    fn transaction_inputs_validate_supplied_prevouts_before_local_comparison() {
        let outpoint = Outpoint {
            transaction_id: TransactionId([3; 32]),
            output_index: 2,
        };
        let local = PreviousOutput {
            outpoint,
            value: Satoshi(10),
            address: None,
        };
        let same_block_outputs = BTreeMap::from([(outpoint, local.clone())]);
        let native = NativeTransaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![bitcoin::TxIn {
                previous_output: bitcoin::OutPoint::new(
                    Txid::from(outpoint.transaction_id),
                    outpoint.output_index,
                ),
                script_sig: ScriptBuf::new(),
                sequence: bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: bitcoin::Witness::new(),
            }],
            output: Vec::new(),
        };
        let original = serde_json::json!({
            "txid": native.compute_txid().to_string(),
            "hex": consensus::serialize(&native).to_lower_hex_string(),
            "vin": [{"txid": outpoint.transaction_id.to_string(), "vout": 2}],
            "vout": [],
        });
        for (prevout, expected_error) in [
            (None, None),
            (Some(Value::Null), None),
            (
                Some(serde_json::json!({"value_satoshis": 10, "address": null})),
                None,
            ),
            (
                Some(serde_json::json!({"value_satoshis": 11, "address": null})),
                Some("Bitcoin input 0 prevout conflicts with an earlier same-block output"),
            ),
            (
                Some(serde_json::json!({"value_satoshis": "invalid", "address": null})),
                Some("Bitcoin compact prevout value is invalid"),
            ),
            (
                Some(serde_json::json!({"value_satoshis": 11})),
                Some("Bitcoin compact prevout address fact is missing"),
            ),
        ] {
            let mut transaction = original.clone();
            if let Some(prevout) = prevout {
                transaction["vin"][0]["prevout"] = prevout;
            }
            let result = Transaction::parse(
                &transaction,
                BlockHeight(10),
                Network::Regtest,
                &same_block_outputs,
            );
            if let Some(message) = expected_error {
                assert_eq!(result.unwrap_err().to_string(), message);
                continue;
            }
            assert_eq!(
                result.unwrap().inputs,
                vec![Input {
                    previous_output: Some(local.clone()),
                }]
            );
        }

        let mut invalid_outpoint = original;
        invalid_outpoint["vin"][0]["vout"] = serde_json::json!(3);
        invalid_outpoint["vin"][0]["prevout"] = serde_json::json!({"value_satoshis": "invalid"});
        let error = Transaction::parse(
            &invalid_outpoint,
            BlockHeight(10),
            Network::Regtest,
            &same_block_outputs,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Bitcoin input 0 outpoint does not match its consensus bytes"
        );
    }
}
