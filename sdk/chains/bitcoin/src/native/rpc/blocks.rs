//! Block reads for indexing, returning retained [`RawBlock`] evidence.

use base::{BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef};
use bitcoin::Transaction as NativeTransaction;
use serde_json::{Value, json};

use super::{
    Client,
    wire::{format_block_hash, parse_block_hash},
};
use crate::native::{
    Error, TransactionId,
    block::{RawBlock, Transaction},
    json::Object,
};

/// `getblockhash`: height out of range.
const HEIGHT_OUT_OF_RANGE: i64 = -8;
/// `getblock` / `getrawtransaction`: no such block or transaction.
const NOT_FOUND: i64 = -5;

impl Error {
    /// A block the node does not currently expose; retry later.
    pub(crate) fn missing_block() -> Self {
        Self::rpc(
            "Bitcoin Core does not currently expose the requested block",
            true,
        )
    }
}

impl<C: json_rpc::Client> Client<C> {
    /// The reference of the best block.
    ///
    /// # Errors
    ///
    /// A retryable [`Error`] when the tip moves while it is read.
    pub async fn tip(&self) -> Result<BlockRef, Error> {
        let height = self.block_count().await?;
        let hash = self.canonical_hash(height).await?.ok_or_else(|| {
            Error::rpc(
                "Bitcoin canonical height changed while its hash was being read",
                true,
            )
        })?;
        self.header(&hash, height).await
    }

    /// The canonical reference at `position`, or `None` past the tip.
    ///
    /// # Errors
    ///
    /// When the node fails or returns an invalid header.
    pub async fn canonical_at(&self, position: BlockPosition) -> Result<Option<BlockRef>, Error> {
        let height = BlockHeight(position.0);
        if height > self.block_count().await? {
            return Ok(None);
        }
        let Some(hash) = self.canonical_hash(height).await? else {
            return Ok(None);
        };
        self.header(&hash, height).await.map(Some)
    }

    /// The canonical hash at `height`; `None` when the height vanished in a
    /// shorter, higher-work reorg.
    ///
    /// # Errors
    ///
    /// When the node fails or returns an invalid hash.
    pub async fn canonical_hash(&self, height: BlockHeight) -> Result<Option<BlockHash>, Error> {
        let raw = self
            .optional("getblockhash", json!([height.0]), HEIGHT_OUT_OF_RANGE)
            .await?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let text: String = raw.deserialize()?;
        parse_block_hash(&text)
            .map(Some)
            .ok_or_else(|| Error::invalid_reply("Bitcoin block hash is invalid"))
    }

    /// The complete block at `position` with every spent output resolved,
    /// validated by [`RawBlock::parse`]; `None` while the node does not
    /// expose it.
    ///
    /// The block is read by hash and the height is read again afterwards, so
    /// a same-height reorg during the read fails instead of mixing blocks.
    ///
    /// # Errors
    ///
    /// When the node fails or returns evidence that does not parse. Invalid
    /// evidence is retryable: a lagging node may still be catching up.
    pub async fn block(&self, position: BlockPosition) -> Result<Option<RawBlock>, Error> {
        let height = BlockHeight(position.0);
        let Some(hash) = self.canonical_hash(height).await? else {
            return Ok(None);
        };
        let params = json!([format_block_hash(&hash)?, 2]);
        let Some(raw) = self.optional("getblock", params, NOT_FOUND).await? else {
            return Ok(None);
        };
        let bytes = self.resolve_prevouts(raw.into_bytes()).await?;
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| Error::invalid_reply("Bitcoin block result is not valid JSON"))?;
        let object = Object::new(&value, "Bitcoin block result", Error::invalid_reply)?;
        let reference = RawBlock::claimed_reference(&object, Some(height), Some(&hash))
            .map_err(Error::unusable)?;
        let block = RawBlock::new(reference, *self.network(), bytes);
        block.parse().map_err(Error::unusable)?;
        if self.canonical_hash(height).await? != Some(hash) {
            return Err(Error::rpc(
                "Bitcoin canonical block changed while it was being fetched",
                true,
            ));
        }
        Ok(Some(block))
    }

    /// The confirmed transaction `id`, decoded and checked against its txid.
    pub(crate) async fn confirmed_transaction(
        &self,
        id: TransactionId,
    ) -> Result<NativeTransaction, Error> {
        let raw = self
            .optional(
                "getrawtransaction",
                json!([id.to_string(), true]),
                NOT_FOUND,
            )
            .await?
            .ok_or_else(|| {
                Error::rpc(
                    format!("Bitcoin previous transaction {id} disappeared during block ingestion"),
                    true,
                )
            })?;
        let value: Value = raw.deserialize()?;
        let object = Object::new(
            &value,
            "Bitcoin getrawtransaction result",
            Error::invalid_reply,
        )?;
        let returned = object
            .string("txid", "Bitcoin previous transaction ID")?
            .parse::<TransactionId>()
            .map_err(|_| object.fail("Bitcoin previous transaction ID is invalid"))?;
        if returned != id {
            return Err(object
                .fail("Bitcoin previous-transaction lookup returned a different transaction ID"));
        }
        let transaction = Transaction::decode(&object, id)?;
        let block = object.string("blockhash", "Bitcoin previous transaction block hash")?;
        parse_block_hash(block).ok_or_else(|| object.fail("Bitcoin block hash is invalid"))?;
        Ok(transaction)
    }

    async fn block_count(&self) -> Result<BlockHeight, Error> {
        let count = self.request("getblockcount", json!([])).await?;
        count
            .as_u64()
            .map(BlockHeight)
            .ok_or_else(|| Error::invalid_reply("Bitcoin block count is invalid"))
    }

    async fn header(&self, hash: &BlockHash, height: BlockHeight) -> Result<BlockRef, Error> {
        let params = json!([format_block_hash(hash)?, true]);
        let value = self.request("getblockheader", params).await?;
        let header = Object::new(
            &value,
            "Bitcoin getblockheader result",
            Error::invalid_reply,
        )?;
        if BlockHeight(header.u64("height", "Bitcoin block-header height")?) != height {
            return Err(header.fail("Bitcoin block header does not match the requested height"));
        }
        let hash_text = header.string("hash", "Bitcoin block-header hash")?;
        let actual = parse_block_hash(hash_text)
            .ok_or_else(|| header.fail("Bitcoin block hash is invalid"))?;
        let parent = match height.0.checked_sub(1) {
            None => None,
            Some(position) => {
                let text = header.string("previousblockhash", "Bitcoin previous block hash")?;
                let hash = parse_block_hash(text)
                    .ok_or_else(|| header.fail("Bitcoin block hash is invalid"))?;
                Some(BlockParent {
                    position: BlockPosition(position),
                    hash,
                })
            }
        };
        let timestamp = header.u64("time", "Bitcoin block-header timestamp")?;
        if &actual != hash {
            return Err(header.fail("Bitcoin header lookup returned a different block hash"));
        }
        Ok(BlockRef {
            position: BlockPosition(height.0),
            height,
            hash: actual,
            parent,
            timestamp: Some(timestamp),
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use bitcoin::{
        Amount, OutPoint, ScriptBuf, Sequence, Transaction as NativeTransaction, TxIn, TxOut,
        Witness, absolute, consensus, hashes::Hash, hex::DisplayHex, transaction::Version,
    };
    use futures_executor::block_on;

    use super::*;
    use crate::native::{
        Address, Network, Satoshi,
        rpc::script::{Reply, Script, Scripted},
    };

    /// Bitcoin Core's display form of the hash whose first byte is `number`.
    pub(crate) fn hash(number: u8) -> String {
        let mut bytes = [0_u8; 32];
        bytes[0] = number;
        bitcoin::BlockHash::from_byte_array(bytes).to_string()
    }

    pub(crate) fn native(
        inputs: Vec<OutPoint>,
        outputs: Vec<(u64, ScriptBuf)>,
    ) -> NativeTransaction {
        NativeTransaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: inputs
                .into_iter()
                .map(|previous_output| TxIn {
                    previous_output,
                    script_sig: ScriptBuf::from_bytes(vec![1, 1]),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
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

    /// The verbosity-2 JSON Bitcoin Core returns for `transaction`.
    pub(crate) fn verbose(transaction: &NativeTransaction) -> Value {
        let inputs: Vec<_> = transaction
            .input
            .iter()
            .map(|input| match input.previous_output.is_null() {
                true => json!({"coinbase": "01"}),
                false => json!({
                    "txid": input.previous_output.txid.to_string(),
                    "vout": input.previous_output.vout,
                }),
            })
            .collect();
        let outputs: Vec<_> = (0_u32..)
            .zip(&transaction.output)
            .map(|(index, output)| {
                json!({
                    "value": Satoshi::from_atomic(output.value.to_sat()).btc(),
                    "n": index,
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

    pub(crate) fn block(transactions: &[&NativeTransaction]) -> Value {
        json!({
            "hash": hash(2), "height": 10, "previousblockhash": hash(3), "time": 100,
            "nTx": transactions.len(),
            "tx": transactions.iter().map(|transaction| verbose(transaction)).collect::<Vec<_>>(),
        })
    }

    pub(crate) fn coinbase() -> NativeTransaction {
        native(
            vec![OutPoint::null()],
            vec![(5_000_000_000, ScriptBuf::new())],
        )
    }

    /// A previous transaction with three outputs, one spending transaction
    /// taking all of them, and a child of the spending transaction.
    pub(crate) fn chain() -> (NativeTransaction, NativeTransaction, NativeTransaction) {
        let previous = native(
            vec![OutPoint::null()],
            vec![
                (
                    123_456_789,
                    ScriptBuf::from_bytes([vec![0x00, 0x14], vec![0x11; 20]].concat()),
                ),
                (
                    50_000,
                    ScriptBuf::from_bytes([vec![0x51, 0x20], vec![0x22; 32]].concat()),
                ),
                // Deliberately large: resolution keeps only value and address.
                (25_000, ScriptBuf::from_bytes(vec![0x61; 65_536])),
            ],
        );
        let spent = (0..3)
            .map(|vout| OutPoint::new(previous.compute_txid(), vout))
            .collect();
        let spending = native(spent, vec![(123_521_789, ScriptBuf::new())]);
        let child = native(
            vec![OutPoint::new(spending.compute_txid(), 0)],
            vec![(123_511_789, ScriptBuf::new())],
        );
        (previous, spending, child)
    }

    pub(crate) fn confirmed(transaction: &NativeTransaction) -> Reply {
        Reply::ok(
            "getrawtransaction",
            json!({
                "txid": transaction.compute_txid().to_string(),
                "hex": consensus::serialize(transaction).to_lower_hex_string(),
                "blockhash": hash(5),
            }),
        )
        .expecting(json!([transaction.compute_txid().to_string(), true]))
    }

    fn fetch(replies: Vec<Reply>) -> (Result<Option<RawBlock>, Error>, Script) {
        let script = Script::node(replies);
        let result = block_on(script.client().block(BlockPosition(10)));
        (result, script)
    }

    #[test]
    fn numbered_block_fetch_parses_transactions_and_rechecks_canonical_hash() {
        let (block, script) = fetch(vec![
            Reply::ok("getblockhash", json!(hash(2))).expecting(json!([10])),
            Reply::ok("getblock", block(&[&coinbase()])).expecting(json!([hash(2), 2])),
            Reply::ok("getblockhash", json!(hash(2))),
        ]);
        script.exhausted();
        let block = block.unwrap().unwrap();
        let reference = block.reference();
        assert_eq!(
            (reference.height, reference.position),
            (BlockHeight(10), BlockPosition(10))
        );
        assert_eq!(Some(reference.hash.clone()), parse_block_hash(&hash(2)));
        assert_eq!(
            reference.parent.as_ref().unwrap().position,
            BlockPosition(9)
        );
        let parsed = block.parse().unwrap();
        let transaction = parsed.transactions().iter().next().unwrap();
        assert!(transaction.coinbase && transaction.inputs[0].previous.is_none());
        assert_eq!(
            transaction.outputs[0].value,
            Satoshi::from_atomic(5_000_000_000)
        );
    }

    #[test]
    fn external_prevouts_are_resolved_once_into_compact_evidence() {
        let (previous, spending, child) = chain();
        let mut value = block(&[&spending, &child]);
        value["tx"][0]["vin"][0]["prevout"] = json!({"untrusted": true});
        value["tx"][1]["vin"][0]["prevout"] = json!({"untrusted": true});
        let (block, script) = fetch(vec![
            Reply::ok("getblockhash", json!(hash(2))),
            Reply::ok("getblock", value),
            confirmed(&previous),
            Reply::ok("getblockhash", json!(hash(2))),
        ]);
        script.exhausted();
        let block = block.unwrap().unwrap();
        let evidence: Value = serde_json::from_slice(block.block()).unwrap();
        let address = Address::from_script(&previous.output[0].script_pubkey, Network::Regtest);
        assert_eq!(
            evidence["tx"][0]["vin"][0]["prevout"],
            json!({"value_satoshis": 123_456_789, "address": address.map(|a| a.to_string())})
        );
        assert_eq!(
            evidence["tx"][0]["vin"][2]["prevout"]["address"],
            Value::Null
        );
        assert!(evidence["tx"][1]["vin"][0].get("prevout").is_none());
        let parsed = block.parse().unwrap();
        let transactions: Vec<_> = parsed.transactions().iter().collect();
        let previous_values: Vec<_> = transactions[0]
            .inputs
            .iter()
            .map(|input| input.previous.as_ref().unwrap().value.units())
            .collect();
        assert_eq!(previous_values, [123_456_789, 50_000, 25_000]);
        assert_eq!(
            transactions[1].inputs[0].previous.as_ref().unwrap().value,
            Satoshi::from_atomic(123_521_789)
        );
    }

    #[test]
    fn claims_are_validated_before_any_previous_transaction_is_requested() {
        let (_, spending, child) = chain();
        let valid = block(&[&spending, &child]);
        let mut wrong_outpoint = valid["tx"][0]["vin"].clone();
        wrong_outpoint[0]["vout"] = json!(1);
        let trailing = format!("{}00", valid["tx"][0]["hex"].as_str().unwrap());
        for (path, claim, message) in [
            (
                "/tx/0/hex",
                Value::Null,
                "Bitcoin raw transaction is missing or invalid",
            ),
            (
                "/tx/0/hex",
                json!("not-hex"),
                "Bitcoin transaction hex is invalid",
            ),
            (
                "/tx/0/hex",
                json!(trailing),
                "Bitcoin transaction consensus bytes are invalid",
            ),
            (
                "/tx/0/txid",
                json!(hash(0)),
                "Bitcoin transaction ID does not match its consensus bytes",
            ),
            (
                "/tx/0/vin",
                Value::Null,
                "Bitcoin transaction inputs must be an array",
            ),
            (
                "/tx/0/vin",
                json!([]),
                "Bitcoin transaction input count does not match its consensus bytes",
            ),
            (
                "/tx/0/vin",
                json!([null, null, null]),
                "Bitcoin transaction input must be an object",
            ),
            (
                "/tx/0/vin",
                wrong_outpoint,
                "Bitcoin input 0 outpoint does not match its consensus bytes",
            ),
            (
                "/tx",
                json!([valid["tx"][1], valid["tx"][0]]),
                concat!(
                    "Bitcoin block transaction spends an output ",
                    "that was not created earlier in the block"
                ),
            ),
            (
                "/tx",
                json!([valid["tx"][0], valid["tx"][0]]),
                "Bitcoin block contains duplicate transaction IDs",
            ),
        ] {
            let mut value = valid.clone();
            *value.pointer_mut(path).unwrap() = claim;
            let (result, script) = fetch(vec![
                Reply::ok("getblockhash", json!(hash(2))),
                Reply::ok("getblock", value),
            ]);
            script.exhausted();
            let error = result.unwrap_err();
            assert_eq!(
                (error.message.as_str(), error.retryable),
                (message, true),
                "{path}"
            );
        }
    }

    #[test]
    fn previous_transactions_must_be_the_confirmed_requested_ones() {
        let (previous, spending, _) = chain();
        let id = previous.compute_txid().to_string();
        let hex = consensus::serialize(&previous).to_lower_hex_string();
        for (reply, message) in [
            (
                Value::Null,
                "Bitcoin getrawtransaction result must be an object",
            ),
            (
                json!({"txid": "bad"}),
                "Bitcoin previous transaction ID is invalid",
            ),
            (
                json!({"txid": hash(9)}),
                "Bitcoin previous-transaction lookup returned a different transaction ID",
            ),
            (
                json!({"txid": id, "hex": "ff"}),
                "Bitcoin transaction consensus bytes are invalid",
            ),
            (
                json!({"txid": id, "hex": hex}),
                "Bitcoin previous transaction block hash is missing or invalid",
            ),
            (
                json!({"txid": id, "hex": hex, "blockhash": "bad"}),
                "Bitcoin block hash is invalid",
            ),
        ] {
            let (result, script) = fetch(vec![
                Reply::ok("getblockhash", json!(hash(2))),
                Reply::ok("getblock", block(&[&spending])),
                Reply::ok("getrawtransaction", reply),
            ]);
            script.exhausted();
            let error = result.unwrap_err();
            assert_eq!((error.message.as_str(), error.retryable), (message, true));
        }
    }

    #[test]
    fn vanished_and_replaced_blocks_are_retryable_or_absent() {
        let (missing, script) = fetch(vec![Reply::failure("getblockhash", -8, "out of range")]);
        assert_eq!(missing, Ok(None));
        script.exhausted();
        let (missing, script) = fetch(vec![
            Reply::ok("getblockhash", json!(hash(2))),
            Reply::failure("getblock", -5, "not found"),
        ]);
        assert_eq!(missing, Ok(None));
        script.exhausted();
        let (replaced, script) = fetch(vec![
            Reply::ok("getblockhash", json!(hash(2))),
            Reply::ok("getblock", block(&[&coinbase()])),
            Reply::ok("getblockhash", json!(hash(4))),
        ]);
        script.exhausted();
        let error = replaced.unwrap_err();
        assert!(error.retryable);
        assert_eq!(
            error.message,
            "Bitcoin canonical block changed while it was being fetched"
        );
    }

    #[test]
    fn tip_and_canonical_references_keep_request_identity_and_coordinates() {
        for height in [0, 10] {
            let header = json!({
                "height": height, "hash": hash(2).to_uppercase(),
                "previousblockhash": hash(3), "time": 100,
            });
            let script = Script::node(vec![
                Reply::ok("getblockcount", json!(10)).expecting(json!([])),
                Reply::ok("getblockhash", json!(hash(2))).expecting(json!([height])),
                Reply::ok("getblockheader", header).expecting(json!([hash(2), true])),
            ]);
            let header = block_on(script.client().canonical_at(BlockPosition(height)))
                .unwrap()
                .unwrap();
            script.exhausted();
            assert_eq!((header.position.0, header.height.0), (height, height));
            assert_eq!(header.timestamp, Some(100));
            assert_eq!(header.parent.is_none(), height == 0);
        }
        let script = Script::node(vec![
            Reply::ok("getblockcount", json!(10)),
            Reply::failure("getblockhash", -8, "gone"),
        ]);
        assert_eq!(
            block_on(script.client().canonical_at(BlockPosition(10))),
            Ok(None)
        );
        let script = Script::node(vec![
            Reply::ok("getblockcount", json!(10)),
            Reply::failure("getblockhash", -8, "gone"),
        ]);
        assert!(block_on(script.client().tip()).unwrap_err().retryable);
    }

    #[test]
    fn header_validation_preserves_error_order_and_retryability() {
        for (header, message) in [
            (
                Value::Null,
                "Bitcoin getblockheader result must be an object",
            ),
            (
                json!({}),
                "Bitcoin block-header height is missing or invalid",
            ),
            (
                json!({"height": 9}),
                "Bitcoin block header does not match the requested height",
            ),
            (
                json!({"height": 10, "hash": "invalid"}),
                "Bitcoin block hash is invalid",
            ),
            (
                json!({"height": 10, "hash": hash(2)}),
                "Bitcoin previous block hash is missing or invalid",
            ),
            (
                json!({"height": 10, "hash": hash(4), "previousblockhash": hash(3)}),
                "Bitcoin block-header timestamp is missing or invalid",
            ),
            (
                json!({"height": 10, "hash": hash(4), "previousblockhash": hash(3), "time": 100}),
                "Bitcoin header lookup returned a different block hash",
            ),
        ] {
            let script = Script::node(vec![
                Reply::ok("getblockcount", json!(10)),
                Reply::ok("getblockhash", json!(hash(2))),
                Reply::ok("getblockheader", header),
            ]);
            let error = block_on(script.client().tip()).unwrap_err();
            assert_eq!((error.message.as_str(), error.retryable), (message, true));
        }
        let error = block_on(
            Script::node(Vec::new())
                .client()
                .header(&BlockHash(vec![2; 31]), BlockHeight(0)),
        )
        .unwrap_err();
        assert_eq!(error.message, "Bitcoin block hash must be 32 bytes");
    }
}
