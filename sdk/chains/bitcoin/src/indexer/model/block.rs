use std::collections::{BTreeMap, BTreeSet};

use bitcoin::{BlockHash as NativeBlockHash, hashes::Hash};
use indexing::{BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef};
use serde_json::{Map, Value};

use crate::Network;

use super::{
    BlockData, ParseError, PreviousOutput, Transaction,
    value::{required_string, required_u64},
};

impl BlockData {
    pub(in crate::indexer) fn parse(
        raw: &[u8],
        expected_height: Option<BlockHeight>,
        expected_hash: Option<&BlockHash>,
        network: Network,
    ) -> Result<Self, ParseError> {
        let value: Value = serde_json::from_slice(raw)
            .map_err(|_| ParseError::new("Bitcoin block result is not valid JSON"))?;
        let object = value
            .as_object()
            .ok_or_else(|| ParseError::new("Bitcoin block result must be an object"))?;
        let height = BlockHeight(required_u64(object, "height", "Bitcoin block height")?);
        if expected_height.is_some_and(|expected| expected != height) {
            return Err(ParseError::new(
                "Bitcoin block height does not match the requested height",
            ));
        }
        let hash = required_string(object, "hash", "Bitcoin block hash")?
            .parse::<NativeBlockHash>()
            .map(|hash| BlockHash(hash.to_byte_array().to_vec()))
            .map_err(|_| ParseError::new("Bitcoin block hash is invalid"))?;
        if expected_hash.is_some_and(|expected| expected != &hash) {
            return Err(ParseError::new(
                "Bitcoin block hash does not match the requested hash",
            ));
        }
        let parent = Self::parse_parent(object, height)?;
        let timestamp = required_u64(object, "time", "Bitcoin block timestamp")?;

        Ok(Self {
            reference: BlockRef {
                position: BlockPosition(height.0),
                height,
                hash,
                parent,
                timestamp: Some(timestamp),
            },
            transactions: Transactions::parse(object, height, network)?.0,
        })
    }

    fn parse_parent(
        object: &Map<String, Value>,
        height: BlockHeight,
    ) -> Result<Option<BlockParent>, ParseError> {
        let has_parent = object
            .get("previousblockhash")
            .is_some_and(|value| !value.is_null());
        if height.0 == 0 && has_parent {
            return Err(ParseError::new(
                "Bitcoin genesis block unexpectedly has a parent hash",
            ));
        }
        if height.0 == 0 {
            Ok(None)
        } else {
            Ok(Some(BlockParent {
                position: BlockPosition(height.0 - 1),
                hash: required_string(object, "previousblockhash", "Bitcoin previous block hash")?
                    .parse::<NativeBlockHash>()
                    .map(|hash| BlockHash(hash.to_byte_array().to_vec()))
                    .map_err(|_| ParseError::new("Bitcoin block hash is invalid"))?,
            }))
        }
    }
}

#[derive(Debug)]
struct Transactions(Vec<Transaction>);

impl Transactions {
    fn parse(
        object: &Map<String, Value>,
        height: BlockHeight,
        network: Network,
    ) -> Result<Self, ParseError> {
        let transaction_values = object
            .get("tx")
            .and_then(Value::as_array)
            .ok_or_else(|| ParseError::new("Bitcoin block transactions must be an array"))?;
        let declared_count = required_u64(object, "nTx", "Bitcoin block transaction count")?;
        if usize::try_from(declared_count).ok() != Some(transaction_values.len()) {
            return Err(ParseError::new(
                "Bitcoin block transaction count does not match its transaction array",
            ));
        }

        let mut transactions = Vec::with_capacity(transaction_values.len());
        let mut transaction_ids = BTreeSet::new();
        let mut same_block_outputs = BTreeMap::new();
        for transaction in transaction_values {
            let transaction =
                Transaction::parse(transaction, height, network, &same_block_outputs)?;
            if !transaction_ids.insert(transaction.id) {
                return Err(ParseError::new(
                    "Bitcoin block contains duplicate transaction IDs",
                ));
            }
            for (index, output) in transaction.outputs.iter().enumerate() {
                let previous = PreviousOutput::from_output(transaction.id, index, output, network)?;
                same_block_outputs.insert(previous.outpoint, previous);
            }
            transactions.push(transaction);
        }

        Ok(Self(transactions))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Satoshi, TransactionId, indexer::Outpoint};
    use bitcoin::{ScriptBuf, Transaction as NativeTransaction, consensus, hex::DisplayHex};

    #[test]
    fn block_transactions_preserve_count_identity_and_same_block_order() {
        let funding = NativeTransaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![bitcoin::TxIn {
                previous_output: bitcoin::OutPoint::null(),
                script_sig: ScriptBuf::new(),
                sequence: bitcoin::Sequence::MAX,
                witness: bitcoin::Witness::new(),
            }],
            output: vec![bitcoin::TxOut {
                value: bitcoin::Amount::from_sat(10),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        let mut spending = funding.clone();
        spending.input[0].previous_output = bitcoin::OutPoint::new(funding.compute_txid(), 0);
        spending.output.clear();
        let funding_json = serde_json::json!({
            "txid": funding.compute_txid().to_string(),
            "hex": consensus::serialize(&funding).to_lower_hex_string(),
            "vin": [{"coinbase": "00"}],
            "vout": [{
                "n": 0,
                "value": "0.00000010".parse::<serde_json::Number>().unwrap(),
                "scriptPubKey": {"hex": ""},
            }],
        });
        let spending_json = serde_json::json!({
            "txid": spending.compute_txid().to_string(),
            "hex": consensus::serialize(&spending).to_lower_hex_string(),
            "vin": [{"txid": funding.compute_txid().to_string(), "vout": 0}],
            "vout": [],
        });
        let original = serde_json::json!({
            "height": 1, "hash": "01".repeat(32), "previousblockhash": "00".repeat(32),
            "time": 2, "nTx": 2, "tx": [funding_json, spending_json],
        });
        let parsed = BlockData::parse(
            &serde_json::to_vec(&original).unwrap(),
            None,
            None,
            Network::Regtest,
        )
        .unwrap();
        assert_eq!(
            parsed.transactions[0].id,
            TransactionId::from(funding.compute_txid())
        );
        assert_eq!(
            parsed.transactions[1].id,
            TransactionId::from(spending.compute_txid())
        );
        assert_eq!(
            parsed.transactions[1].inputs[0].previous_output,
            Some(PreviousOutput {
                outpoint: Outpoint {
                    transaction_id: TransactionId::from(funding.compute_txid()),
                    output_index: 0,
                },
                value: Satoshi(10),
                address: None,
            })
        );

        let mut invalid_duplicate = original["tx"][0].clone();
        invalid_duplicate["vout"] = serde_json::json!([]);
        for (transactions, count, message) in [
            (
                Value::Null,
                Value::Null,
                "Bitcoin block transactions must be an array",
            ),
            (
                serde_json::json!([null]),
                serde_json::json!(2),
                "Bitcoin block transaction count does not match its transaction array",
            ),
            (
                serde_json::json!([original["tx"][0], original["tx"][0]]),
                serde_json::json!(2),
                "Bitcoin block contains duplicate transaction IDs",
            ),
            (
                serde_json::json!([original["tx"][0], invalid_duplicate]),
                serde_json::json!(2),
                "Bitcoin transaction output count does not match its consensus bytes",
            ),
            (
                serde_json::json!([original["tx"][1], original["tx"][0]]),
                serde_json::json!(2),
                "Bitcoin input 0 has no resolved previous output",
            ),
        ] {
            let mut block = original.clone();
            block["tx"] = transactions;
            block["nTx"] = count;
            let error = BlockData::parse(
                &serde_json::to_vec(&block).unwrap(),
                None,
                None,
                Network::Regtest,
            )
            .unwrap_err();
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn genesis_parent_rules_preserve_identity_validation_order() {
        let original = serde_json::json!({
            "height": 0, "hash": "00".repeat(32), "time": 2, "tx": [], "nTx": 0,
        });
        for parent in [None, Some(Value::Null)] {
            let mut value = original.clone();
            if let Some(parent) = parent {
                value["previousblockhash"] = parent;
            }
            let block = BlockData::parse(
                &serde_json::to_vec(&value).unwrap(),
                Some(BlockHeight(0)),
                Some(&BlockHash(vec![0; 32])),
                Network::Regtest,
            )
            .unwrap();
            assert_eq!(block.reference.parent, None);
            assert_eq!(block.reference.position, BlockPosition(0));
        }

        for parent in [serde_json::json!("invalid"), serde_json::json!(false)] {
            let mut value = original.clone();
            value["previousblockhash"] = parent;
            value["time"] = Value::Null;
            let raw = serde_json::to_vec(&value).unwrap();
            let error = BlockData::parse(&raw, None, None, Network::Regtest).unwrap_err();
            assert_eq!(
                error.to_string(),
                "Bitcoin genesis block unexpectedly has a parent hash"
            );
            let error =
                BlockData::parse(&raw, None, Some(&BlockHash(vec![1; 32])), Network::Regtest)
                    .unwrap_err();
            assert_eq!(
                error.to_string(),
                "Bitcoin block hash does not match the requested hash"
            );
        }
    }

    #[test]
    fn block_hashes_keep_native_byte_order_and_case_acceptance() {
        let hash = "1F1E1D1C1B1A191817161514131211100F0E0D0C0B0A09080706050403020100";
        let parent = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let raw = serde_json::json!({
            "height": 1, "hash": hash, "previousblockhash": parent,
            "time": 2, "tx": [], "nTx": 0,
        });
        let block = BlockData::parse(
            &serde_json::to_vec(&raw).unwrap(),
            None,
            None,
            Network::Regtest,
        )
        .unwrap();
        assert_eq!(block.reference.hash, BlockHash((0_u8..32).collect()));
        assert_eq!(
            block.reference.parent.unwrap().hash,
            BlockHash((0_u8..32).rev().collect())
        );
    }

    #[test]
    fn block_hash_errors_keep_context_and_height_validation_precedence() {
        let invalid = serde_json::json!({"height": 1, "hash": "invalid"});
        let raw = serde_json::to_vec(&invalid).unwrap();
        let error =
            BlockData::parse(&raw, Some(BlockHeight(2)), None, Network::Regtest).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Bitcoin block height does not match the requested height"
        );
        let error = BlockData::parse(&raw, None, None, Network::Regtest).unwrap_err();
        assert_eq!(error.to_string(), "Bitcoin block hash is invalid");

        for field in ["hash", "previousblockhash"] {
            for value in ["00".repeat(31), "00".repeat(33), "gg".repeat(32)] {
                let mut block = serde_json::json!({
                    "height": 1, "hash": "00".repeat(32), "previousblockhash": "00".repeat(32),
                    "time": 2, "tx": [], "nTx": 0,
                });
                block[field] = Value::String(value);
                let error = BlockData::parse(
                    &serde_json::to_vec(&block).unwrap(),
                    None,
                    None,
                    Network::Regtest,
                )
                .unwrap_err();
                assert_eq!(error.to_string(), "Bitcoin block hash is invalid");
            }
        }
    }
}
