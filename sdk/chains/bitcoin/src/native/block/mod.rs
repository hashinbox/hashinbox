//! Blocks as retained evidence ([`RawBlock`]) and as parsed facts ([`Block`]).
//!
//! `RawBlock` → [`RawBlock::parse`] → [`Block`] → [`Transaction`] →
//! [`Transaction::movements`] → [`Movement`]. Parsing happens only here.

mod output;
mod transaction;

use std::collections::{BTreeMap, BTreeSet};

use base::{BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef};
use serde_json::Value;

pub use output::{COINBASE_MATURITY, Outpoint, Output, Utxo};
pub use transaction::{Fee, Input, Movement, Transaction};

use crate::native::{Error, Network, json::Object, rpc::wire::parse_block_hash};

/// One parsed block: its reference and transactions in block order.
pub type Block = base::Block<Transaction>;
/// Transactions of one block in block order, with unique ids.
pub type Transactions = base::Transactions<Transaction>;

/// Exact source evidence for one block, retained for replay and reorg recovery.
///
/// The payload is Bitcoin Core's `getblock` verbosity-2 JSON with every
/// input's previous output resolved into a compact `prevout` (value and
/// address), plus the network that gives addresses their text. Keeping bytes
/// rather than parsed facts makes replay independent of this parser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawBlock {
    reference: BlockRef,
    network: Network,
    block: Vec<u8>,
}

impl RawBlock {
    /// Pairs evidence with the reference it claims; [`Self::parse`] checks both agree.
    #[must_use]
    pub const fn new(reference: BlockRef, network: Network, block: Vec<u8>) -> Self {
        Self {
            reference,
            network,
            block,
        }
    }

    /// The block this evidence claims to be.
    #[must_use]
    pub const fn reference(&self) -> &BlockRef {
        &self.reference
    }

    /// The network whose address encoding the evidence uses.
    #[must_use]
    pub const fn network(&self) -> Network {
        self.network
    }

    /// The enriched `getblock` JSON bytes.
    #[must_use]
    pub fn block(&self) -> &[u8] {
        &self.block
    }

    /// Validates the evidence and decodes it into a [`Block`].
    ///
    /// # Errors
    ///
    /// [`crate::native::ErrorKind::InvalidBlock`] naming the first violation.
    pub fn parse(&self) -> Result<Block, Error> {
        let value: Value = serde_json::from_slice(&self.block)
            .map_err(|_| Error::invalid_block("Bitcoin block result is not valid JSON"))?;
        let object = Object::new(&value, "Bitcoin block result", Error::invalid_block)?;
        let reference = Self::claimed_reference(&object, None, None)?;
        if reference != self.reference {
            return Err(Error::invalid_block(
                "retained Bitcoin block reference does not match its raw payload",
            ));
        }
        let transactions = Self::transactions(&object, reference.height.0, self.network)?;
        let transactions = Transactions::new(transactions).map_err(|_| {
            Error::invalid_block("Bitcoin block contains duplicate transaction IDs")
        })?;
        Ok(Block::new(&self.network, reference, transactions))
    }

    fn transactions(
        object: &Object<'_>,
        height: u64,
        network: Network,
    ) -> Result<Vec<Transaction>, Error> {
        let values = object.array("tx", "Bitcoin block transactions")?;
        let declared = object.u64("nTx", "Bitcoin block transaction count")?;
        if usize::try_from(declared).ok() != Some(values.len()) {
            return Err(
                object.fail("Bitcoin block transaction count does not match its transaction array")
            );
        }
        let mut earlier = BTreeMap::new();
        let mut ids = BTreeSet::new();
        let mut spent = BTreeSet::new();
        let mut transactions = Vec::with_capacity(values.len());
        for value in values {
            let transaction = Transaction::parse(value, height, network, &earlier)?;
            if !ids.insert(*transaction.id()) {
                return Err(object.fail("Bitcoin block contains duplicate transaction IDs"));
            }
            let mut spends = transaction
                .inputs
                .iter()
                .filter(|input| input.previous.is_some());
            if !spends.all(|input| spent.insert(input.outpoint)) {
                return Err(object.fail("Bitcoin block spends the same outpoint more than once"));
            }
            earlier.extend(transaction.created());
            transactions.push(transaction);
        }
        Ok(transactions)
    }
}

impl Transaction {
    /// This transaction's outputs keyed by the outpoints later spenders name.
    fn created(&self) -> impl Iterator<Item = (Outpoint, Output)> + '_ {
        (0_u32..).zip(&self.outputs).map(|(output_index, output)| {
            let outpoint = Outpoint {
                transaction_id: *self.id(),
                output_index,
            };
            (outpoint, output.clone())
        })
    }
}

impl RawBlock {
    /// The reference a `getblock` result claims, checked against the
    /// requested height and hash when given.
    pub(crate) fn claimed_reference(
        object: &Object<'_>,
        height: Option<BlockHeight>,
        hash: Option<&BlockHash>,
    ) -> Result<BlockRef, Error> {
        let actual = BlockHeight(object.u64("height", "Bitcoin block height")?);
        if height.is_some_and(|expected| expected != actual) {
            return Err(object.fail("Bitcoin block height does not match the requested height"));
        }
        let actual_hash = Self::hash(object, object.string("hash", "Bitcoin block hash")?)?;
        if hash.is_some_and(|expected| expected != &actual_hash) {
            return Err(object.fail("Bitcoin block hash does not match the requested hash"));
        }
        let parent = Self::parent(object, actual)?;
        Ok(BlockRef {
            position: BlockPosition(actual.0),
            height: actual,
            hash: actual_hash,
            parent,
            timestamp: Some(object.u64("time", "Bitcoin block timestamp")?),
        })
    }

    fn parent(object: &Object<'_>, height: BlockHeight) -> Result<Option<BlockParent>, Error> {
        let claimed = object
            .get("previousblockhash")
            .is_some_and(|value| !value.is_null());
        let Some(position) = height.0.checked_sub(1) else {
            if claimed {
                return Err(object.fail("Bitcoin genesis block unexpectedly has a parent hash"));
            }
            return Ok(None);
        };
        let hash = object.string("previousblockhash", "Bitcoin previous block hash")?;
        Ok(Some(BlockParent {
            position: BlockPosition(position),
            hash: Self::hash(object, hash)?,
        }))
    }

    fn hash(object: &Object<'_>, text: &str) -> Result<BlockHash, Error> {
        parse_block_hash(text).ok_or_else(|| object.fail("Bitcoin block hash is invalid"))
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::{
        Amount, OutPoint, ScriptBuf, Sequence, Transaction as NativeTransaction, TxIn, TxOut, Txid,
        Witness, absolute, consensus, hex::DisplayHex, transaction::Version,
    };
    use serde_json::json;

    use super::*;
    use crate::native::{Address, ErrorKind, Satoshi};

    fn btc(satoshis: u64) -> Value {
        Value::Number(Satoshi::from_atomic(satoshis).btc())
    }

    fn native(inputs: Vec<OutPoint>, outputs: Vec<(u64, ScriptBuf)>) -> NativeTransaction {
        NativeTransaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: inputs
                .into_iter()
                .map(|previous_output| TxIn {
                    previous_output,
                    script_sig: ScriptBuf::from_bytes(vec![1, 1]),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                })
                .collect(),
            output: outputs
                .into_iter()
                .map(|(value, script_pubkey)| TxOut {
                    value: Amount::from_sat(value),
                    script_pubkey,
                })
                .collect(),
        }
    }

    /// Verbosity-2 JSON; `prevouts[i]` becomes input `i`'s `prevout` when set.
    fn json(transaction: &NativeTransaction, prevouts: &[Option<Value>]) -> Value {
        let inputs: Vec<_> = transaction
            .input
            .iter()
            .enumerate()
            .map(|(index, input)| {
                if input.previous_output.is_null() {
                    return json!({"coinbase": "01"});
                }
                let mut value = json!({
                    "txid": input.previous_output.txid.to_string(),
                    "vout": input.previous_output.vout,
                });
                if let Some(Some(prevout)) = prevouts.get(index) {
                    value["prevout"] = prevout.clone();
                }
                value
            })
            .collect();
        let outputs: Vec<_> = transaction
            .output
            .iter()
            .enumerate()
            .map(|(index, output)| {
                json!({
                    "n": index,
                    "value": btc(output.value.to_sat()),
                    "scriptPubKey": {"hex": output.script_pubkey.as_bytes().to_lower_hex_string()},
                })
            })
            .collect();
        json!({
            "txid": transaction.compute_txid().to_string(),
            "hex": consensus::serialize(transaction).to_lower_hex_string(),
            "vin": inputs,
            "vout": outputs,
        })
    }

    fn block(transactions: Vec<Value>) -> Value {
        json!({
            "height": 10, "hash": "aa".repeat(32), "previousblockhash": "bb".repeat(32),
            "time": 100, "nTx": transactions.len(), "tx": transactions,
        })
    }

    fn raw(value: &Value) -> RawBlock {
        let object = Object::new(value, "block", Error::invalid_block).unwrap();
        let reference = RawBlock::claimed_reference(&object, None, None).unwrap();
        RawBlock::new(
            reference,
            Network::Regtest,
            serde_json::to_vec(value).unwrap(),
        )
    }

    fn parse(value: &Value) -> Result<Block, Error> {
        raw(value).parse()
    }

    fn reference(value: &Value) -> Result<BlockRef, Error> {
        let object = Object::new(value, "Bitcoin block result", Error::invalid_block)?;
        RawBlock::claimed_reference(&object, None, None)
    }

    fn p2wpkh(byte: u8) -> (Address, ScriptBuf) {
        let script = ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array([byte; 20]));
        let address = Address::from_script(&script, Network::Regtest).unwrap();
        (address, script)
    }

    fn compact(value: u64, address: Option<&Address>) -> Option<Value> {
        Some(json!({"value_satoshis": value, "address": address.map(Address::as_str)}))
    }

    use bitcoin::hashes::Hash as _;

    #[test]
    fn same_block_spends_resolve_without_prevouts_and_fees_name_one_payer() {
        let (address, script) = p2wpkh(7);
        let funding = native(vec![OutPoint::null()], vec![(10_000, script.clone())]);
        let spending = native(
            vec![OutPoint::new(funding.compute_txid(), 0)],
            vec![(9_000, ScriptBuf::new())],
        );
        let block = parse(&block(vec![json(&funding, &[]), json(&spending, &[])])).unwrap();
        let transactions: Vec<_> = block.transactions().iter().collect();
        assert!(transactions[0].coinbase);
        assert_eq!(transactions[0].fee(), None);
        let previous = transactions[1].inputs[0].previous.as_ref().unwrap();
        assert_eq!(previous.value, Satoshi::from_atomic(10_000));
        assert_eq!(previous.address.as_ref(), Some(&address));
        assert_eq!(
            transactions[1].fee(),
            Some(&Fee {
                amount: Satoshi::from_atomic(1_000),
                payer: Some(address.clone()),
            })
        );
        let movements = transactions[1].movements();
        assert_eq!(movements.len(), 2);
        assert_eq!(movements[0].kind, MovementKind::Input);
        assert_eq!(movements[0].from, Some(address));
        assert_eq!(
            movements[1].id,
            format!("{}:vout:0", spending.compute_txid())
        );
    }

    use base::MovementKind;

    #[test]
    fn block_level_violations_keep_their_order() {
        let funding = native(vec![OutPoint::null()], vec![(10, ScriptBuf::new())]);
        let spending = native(vec![OutPoint::new(funding.compute_txid(), 0)], Vec::new());
        let (funding, spending) = (json(&funding, &[]), json(&spending, &[]));
        let mut no_outputs = funding.clone();
        no_outputs["vout"] = json!([]);
        for (transactions, count, message) in [
            (
                Value::Null,
                Value::Null,
                "Bitcoin block transactions must be an array",
            ),
            (
                json!([null]),
                json!(2),
                "Bitcoin block transaction count does not match its transaction array",
            ),
            (
                json!([funding, funding]),
                json!(2),
                "Bitcoin block contains duplicate transaction IDs",
            ),
            (
                json!([funding, no_outputs]),
                json!(2),
                "Bitcoin transaction output count does not match its consensus bytes",
            ),
            (
                json!([spending, funding]),
                json!(2),
                "Bitcoin input 0 has no resolved previous output",
            ),
        ] {
            let mut value = block(Vec::new());
            value["tx"] = transactions;
            value["nTx"] = count;
            let error = parse(&value).unwrap_err();
            assert_eq!(
                (error.kind, error.message.as_str()),
                (ErrorKind::InvalidBlock, message)
            );
        }
    }

    #[test]
    fn head_rules_preserve_genesis_hash_order_and_retained_reference() {
        let genesis = json!({"height": 0, "hash": "00".repeat(32), "time": 2, "tx": [], "nTx": 0});
        for parent in [None, Some(Value::Null)] {
            let mut value = genesis.clone();
            if let Some(parent) = parent {
                value["previousblockhash"] = parent;
            }
            assert_eq!(reference(&value).unwrap().parent, None);
        }
        let mut value = genesis;
        value["previousblockhash"] = json!(false);
        value["time"] = Value::Null;
        let error = reference(&value).unwrap_err();
        assert_eq!(
            error.message,
            "Bitcoin genesis block unexpectedly has a parent hash"
        );
        let object = Object::new(&value, "block", Error::invalid_block).unwrap();
        let other = BlockHash(vec![1; 32]);
        let error = RawBlock::claimed_reference(&object, None, Some(&other)).unwrap_err();
        assert_eq!(
            error.message,
            "Bitcoin block hash does not match the requested hash"
        );

        let value = block(Vec::new());
        let mut retained = raw(&value);
        retained.reference.timestamp = Some(1);
        let error = retained.parse().unwrap_err();
        assert_eq!(
            error.message,
            "retained Bitcoin block reference does not match its raw payload"
        );
    }

    #[test]
    fn block_hashes_keep_native_byte_order_and_reject_malformed_text() {
        let hash = "1F1E1D1C1B1A191817161514131211100F0E0D0C0B0A09080706050403020100";
        let parent = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let mut value = block(Vec::new());
        value["hash"] = json!(hash);
        value["previousblockhash"] = json!(parent);
        let head = reference(&value).unwrap();
        assert_eq!(head.hash, BlockHash((0_u8..32).collect()));
        assert_eq!(
            head.parent.unwrap().hash,
            BlockHash((0_u8..32).rev().collect())
        );
        for field in ["hash", "previousblockhash"] {
            for text in ["00".repeat(31), "00".repeat(33), "gg".repeat(32)] {
                let mut value = block(Vec::new());
                value[field] = Value::String(text);
                assert_eq!(
                    reference(&value).unwrap_err().message,
                    "Bitcoin block hash is invalid"
                );
            }
        }
        let mut value = json!({"height": 1, "hash": "invalid"});
        let object = Object::new(&value, "block", Error::invalid_block).unwrap();
        let error = RawBlock::claimed_reference(&object, Some(BlockHeight(2)), None).unwrap_err();
        assert_eq!(
            error.message,
            "Bitcoin block height does not match the requested height"
        );
        value["height"] = json!(2);
        assert_eq!(
            reference(&value).unwrap_err().message,
            "Bitcoin block hash is invalid"
        );
    }

    #[test]
    fn transaction_claims_are_checked_against_consensus_bytes_in_order() {
        let transaction = native(
            vec![OutPoint::null()],
            vec![
                (10, ScriptBuf::from_bytes(vec![0x6a])),
                (20, ScriptBuf::new()),
            ],
        );
        let original = json(&transaction, &[]);
        for (path, invalid, message) in [
            (
                "/txid",
                json!("00".repeat(32)),
                "Bitcoin transaction ID does not match its consensus bytes",
            ),
            ("/txid", json!("ff"), "Bitcoin transaction ID is invalid"),
            (
                "/txid",
                Value::Null,
                "Bitcoin transaction ID is missing or invalid",
            ),
            (
                "/hex",
                json!("ff"),
                "Bitcoin transaction consensus bytes are invalid",
            ),
            ("/hex", json!("zz"), "Bitcoin transaction hex is invalid"),
            (
                "/vin",
                Value::Null,
                "Bitcoin transaction inputs must be an array",
            ),
            (
                "/vin",
                json!([]),
                "Bitcoin transaction input count does not match its consensus bytes",
            ),
            (
                "/vin/0",
                Value::Null,
                "Bitcoin transaction input must be an object",
            ),
            (
                "/vin/0/coinbase",
                Value::Null,
                "Bitcoin null input is not a valid coinbase input",
            ),
            (
                "/vout",
                Value::Null,
                "Bitcoin transaction outputs must be an array",
            ),
            (
                "/vout",
                json!([]),
                "Bitcoin transaction output count does not match its consensus bytes",
            ),
            (
                "/vout/0",
                Value::Null,
                "Bitcoin transaction output must be an object",
            ),
            (
                "/vout/0/n",
                json!(1),
                "Bitcoin transaction output index does not match its position",
            ),
            (
                "/vout/0/value",
                json!(0),
                "Bitcoin output value does not match its consensus bytes",
            ),
            (
                "/vout/0/scriptPubKey",
                Value::Null,
                "Bitcoin output scriptPubKey is missing",
            ),
            (
                "/vout/0/scriptPubKey/hex",
                Value::Null,
                "Bitcoin scriptPubKey hex is missing or invalid",
            ),
            (
                "/vout/0/scriptPubKey/hex",
                json!("6"),
                "Bitcoin scriptPubKey hex is invalid",
            ),
            (
                "/vout/0/scriptPubKey/hex",
                json!("6b"),
                "Bitcoin output script does not match its consensus bytes",
            ),
        ] {
            let mut value = original.clone();
            // Earlier violations must win over this later one.
            value["vout"][1]["value"] = json!(0);
            *value.pointer_mut(path).unwrap() = invalid;
            let error = parse(&block(vec![value])).unwrap_err();
            assert_eq!(error.message, message, "{path}");
        }
        let block_value = block(vec![Value::Null]);
        assert_eq!(
            parse(&block_value).unwrap_err().message,
            "Bitcoin transaction must be an object"
        );
    }

    /// Every input survives the parse as the block states it. The coinbase
    /// input keeps its null outpoint and its sequence and spends nothing;
    /// every other input keeps its outpoint and sequence, in consensus order,
    /// beside the output it spends.
    #[test]
    fn a_coinbase_input_spends_nothing_and_other_inputs_keep_outpoint_and_sequence() {
        let (address, script) = p2wpkh(7);
        let mut funding = native(vec![OutPoint::null()], vec![(10_000, script)]);
        funding.input[0].sequence = Sequence(0xffff_fffe);
        let elsewhere = OutPoint::new(Txid::from_byte_array([3; 32]), 2);
        let mut spending = native(
            vec![elsewhere, OutPoint::new(funding.compute_txid(), 0)],
            vec![(9_000, ScriptBuf::new())],
        );
        spending.input[0].sequence = Sequence::ENABLE_RBF_NO_LOCKTIME;
        spending.input[1].sequence = Sequence(5);
        let prevouts = [compact(400, Some(&address)), None];
        let value = block(vec![json(&funding, &[]), json(&spending, &prevouts)]);
        let block = parse(&value).unwrap();
        let transactions: Vec<_> = block.transactions().iter().collect();

        assert!(transactions[0].coinbase);
        assert_eq!(
            transactions[0].inputs,
            [Input {
                outpoint: Outpoint::from(OutPoint::null()),
                sequence: 0xffff_fffe,
                previous: None,
            }]
        );
        assert!(!transactions[1].coinbase);
        let spent: Vec<_> = transactions[1]
            .inputs
            .iter()
            .map(|input| {
                let previous = input.previous.as_ref().expect("a resolved previous output");
                (input.outpoint, input.sequence, previous.value)
            })
            .collect();
        assert_eq!(
            spent,
            [
                (
                    Outpoint::from(elsewhere),
                    Sequence::ENABLE_RBF_NO_LOCKTIME.to_consensus_u32(),
                    Satoshi::from_atomic(400),
                ),
                (
                    Outpoint {
                        transaction_id: *transactions[0].id(),
                        output_index: 0,
                    },
                    5,
                    Satoshi::from_atomic(10_000),
                ),
            ]
        );
    }

    #[test]
    fn inputs_keep_coinbase_outpoint_and_prevout_rules() {
        let (address, script) = p2wpkh(7);
        let wrong_network = Address::from_script(&script, Network::Mainnet).unwrap();
        let spending = native(
            vec![OutPoint::new(Txid::from_byte_array([3; 32]), 2)],
            vec![(9, ScriptBuf::new())],
        );
        let verbose = json!({
            "value": btc(10), "scriptPubKey": {"hex": script.as_bytes().to_lower_hex_string()},
            "height": 9, "generated": false,
        });
        for (prevout, patch, message) in [
            (
                None,
                None,
                "Bitcoin input 0 has no resolved previous output",
            ),
            (
                Some(json!({"value_satoshis": null})),
                None,
                "Bitcoin compact prevout value is invalid",
            ),
            (
                Some(json!({"value_satoshis": 10})),
                None,
                "Bitcoin compact prevout address fact is missing",
            ),
            (
                Some(json!({"value_satoshis": 10, "address": false})),
                None,
                "Bitcoin compact prevout address fact is invalid",
            ),
            (
                compact(10, Some(&wrong_network)),
                None,
                "Bitcoin compact prevout address is invalid or wrong-network",
            ),
            (
                Some(json!({"value_satoshis": 10, "address": address.as_str().to_uppercase()})),
                None,
                "Bitcoin compact prevout address is not canonical",
            ),
            (
                Some(verbose.clone()),
                Some(("/height", json!(11))),
                "Bitcoin input previous output was created after the spending block",
            ),
            (
                Some(verbose.clone()),
                Some(("/generated", Value::Null)),
                "Bitcoin prevout coinbase flag is missing or invalid",
            ),
            (
                Some(verbose.clone()),
                Some(("/scriptPubKey", Value::Null)),
                "Bitcoin prevout scriptPubKey is missing",
            ),
        ] {
            let mut prevout = prevout;
            if let (Some(prevout), Some((path, value))) = (&mut prevout, patch) {
                *prevout.pointer_mut(path).unwrap() = value;
            }
            let spending = json(&spending, &[prevout]);
            let error = parse(&block(vec![spending])).unwrap_err();
            assert_eq!(error.message, message);
        }
        let parsed = parse(&block(vec![json(&spending, &[Some(verbose)])])).unwrap();
        let input = &parsed.transactions().iter().next().unwrap().inputs[0];
        assert_eq!(
            input.previous.as_ref().unwrap().address,
            Some(address.clone())
        );
        assert_eq!(input.outpoint.output_index, 2);
    }

    #[test]
    fn outputs_may_not_exceed_resolved_inputs() {
        let (address, _) = p2wpkh(7);
        let spending = native(
            vec![OutPoint::new(Txid::from_byte_array([3; 32]), 2)],
            vec![(12, ScriptBuf::new())],
        );
        let value = block(vec![json(&spending, &[compact(11, Some(&address))])]);
        assert_eq!(
            parse(&value).unwrap_err().message,
            "Bitcoin transaction outputs exceed its resolved inputs"
        );
    }

    #[test]
    fn supplied_prevouts_must_agree_with_earlier_same_block_outputs() {
        let (address, script) = p2wpkh(7);
        let funding = native(vec![OutPoint::null()], vec![(10, script)]);
        let spending = native(vec![OutPoint::new(funding.compute_txid(), 0)], Vec::new());
        for (prevout, message) in [
            (Some(Value::Null), None),
            (compact(10, Some(&address)), None),
            (
                compact(11, Some(&address)),
                Some("Bitcoin input 0 prevout conflicts with an earlier same-block output"),
            ),
            (
                compact(10, None),
                Some("Bitcoin input 0 prevout conflicts with an earlier same-block output"),
            ),
        ] {
            let value = block(vec![json(&funding, &[]), json(&spending, &[prevout])]);
            match message {
                Some(message) => assert_eq!(parse(&value).unwrap_err().message, message),
                None => assert!(parse(&value).is_ok()),
            }
        }
        let mut wrong_outpoint = json(&spending, &[]);
        wrong_outpoint["vin"][0]["vout"] = json!(3);
        let value = block(vec![json(&funding, &[]), wrong_outpoint]);
        assert_eq!(
            parse(&value).unwrap_err().message,
            "Bitcoin input 0 outpoint does not match its consensus bytes"
        );
    }

    #[test]
    fn duplicate_spends_and_coinbase_inputs_are_invalid_blocks() {
        let (address, script) = p2wpkh(7);
        let outpoint = OutPoint::new(Txid::from_byte_array([9; 32]), 1);
        let first = native(vec![outpoint], vec![(1_000, script.clone())]);
        let second = native(vec![outpoint], vec![(900, script)]);
        let prevout = compact(2_000, Some(&address));
        let value = block(vec![
            json(&first, std::slice::from_ref(&prevout)),
            json(&second, &[prevout]),
        ]);
        assert_eq!(
            parse(&value).unwrap_err().message,
            "Bitcoin block spends the same outpoint more than once"
        );
        let mixed = native(vec![OutPoint::null(), outpoint], Vec::new());
        let mut claims = json(&mixed, &[]);
        claims["vin"][1]["prevout"] = compact(1, None).unwrap();
        let error = parse(&block(vec![claims])).unwrap_err();
        // A coinbase has exactly one input, so a null outpoint beside others is invalid.
        assert_eq!(
            error.message,
            "Bitcoin null input is not a valid coinbase input"
        );
    }

    /// The parsed block belongs to the network its evidence was retained
    /// for, and to no other: the same bytes retained for two networks parse
    /// into two different blocks.
    #[test]
    fn a_parsed_block_carries_the_network_of_its_evidence() {
        let value = block(Vec::new());
        let regtest = parse(&value).unwrap();
        assert_eq!(regtest.network().chain, crate::native::CHAIN_NAME);
        assert_eq!(regtest.network().name, "regtest");

        let retained = raw(&value);
        let elsewhere = RawBlock::new(
            retained.reference().clone(),
            Network::Testnet4,
            retained.block().to_vec(),
        );
        let testnet = elsewhere.parse().unwrap();
        assert_eq!(testnet.network().name, "testnet4");
        assert_ne!(regtest, testnet);
    }

    #[test]
    fn parsing_is_deterministic_so_retained_evidence_replays() {
        let (address, script) = p2wpkh(7);
        let spending = native(
            vec![OutPoint::new(Txid::from_byte_array([3; 32]), 0)],
            vec![(900, script)],
        );
        let raw = raw(&block(vec![json(
            &spending,
            &[compact(1_000, Some(&address))],
        )]));
        let replayed = RawBlock::new(raw.reference().clone(), raw.network(), raw.block().to_vec());
        assert_eq!(raw.parse().unwrap(), replayed.parse().unwrap());
    }
}
