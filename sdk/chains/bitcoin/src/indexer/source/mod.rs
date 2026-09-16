use std::collections::{BTreeMap, BTreeSet};

use futures_util::{StreamExt, stream};
use indexing::{
    BlockHash, BlockHeight, BlockPosition, BlockRef, BlockSource, BoxFuture, ChainId, IndexScope,
    SourceError,
};
use json_rpc::Client;
use serde_json::Value;

use crate::{Network, TransactionId};

use super::Block;
use crate::rpc::{
    Client as RpcClient, CoreConfig, format_bitcoin_block_hash, parse_bitcoin_block_hash,
    source_error,
};

// Every non-coinbase input consumes at least a 36-byte outpoint, one-byte
// empty-script length, and four-byte sequence in non-witness serialization.
// At four weight units per byte, a 4,000,000-WU block can therefore contain at
// most floor(4,000,000 / 164) = 24,390 external prevouts even before mandatory
// block and transaction overhead. This deliberately loose ceiling remains
// complete for every consensus-valid block.
const MAX_EXTERNAL_PREVOUTS_PER_BLOCK: usize = 25_000;
const MAX_IN_FLIGHT_PREVOUT_REQUESTS: usize = 4;
const MAX_COMPACT_ADDRESS_BYTES: usize = 128;
const MAX_COMPACT_PREVOUT_JSON_BYTES: usize = 192;
const MAX_COMPACT_PREVOUT_TOTAL_BYTES: usize =
    MAX_EXTERNAL_PREVOUTS_PER_BLOCK * MAX_COMPACT_PREVOUT_JSON_BYTES;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub scope: IndexScope,
    pub network: Network,
    pub expected_genesis_hash: Option<BlockHash>,
}

impl Config {
    pub fn validate(&self) -> Result<(), SourceError> {
        if self.scope.chain != ChainId(crate::CHAIN.to_owned()) {
            return Err(source_error(
                "Bitcoin index source scope must use the bitcoin chain ID",
                false,
            ));
        }
        if self.scope.network != self.network.canonical_name() {
            return Err(source_error(
                "Bitcoin index source scope network does not match configuration",
                false,
            ));
        }
        CoreConfig {
            expected_network: self.network,
            expected_genesis_hash: self.expected_genesis_hash.clone(),
        }
        .validate()
    }
}

/// Canonical numbered-block source using bounded Bitcoin Core 31 RPC results.
///
/// The source fetches the block at verbosity 2, then resolves only external
/// previous transactions through at most four concurrent, transport-bounded calls.
/// Full historical scripts are validated and discarded at the source boundary;
/// parsed blocks contain only the value/address facts interpretation needs.
pub struct Blocks<C> {
    client: RpcClient<C>,
    config: Config,
}

impl<C> Blocks<C>
where
    C: Client,
{
    pub async fn connect(client: C, config: Config) -> Result<Self, SourceError> {
        config.validate()?;
        let client = RpcClient::connect_with(
            client,
            CoreConfig {
                expected_network: config.network,
                expected_genesis_hash: config.expected_genesis_hash.clone(),
            },
        )
        .await?;
        Ok(Self { client, config })
    }

    /// Builds block reads over an already connected shared RPC client.
    pub fn from_client(client: RpcClient<C>, config: Config) -> Result<Self, SourceError> {
        config.validate()?;
        let expected = client.config();
        if expected.expected_network != config.network
            || expected.expected_genesis_hash != config.expected_genesis_hash
        {
            return Err(source_error(
                "Bitcoin block adapter configuration does not match its RPC client",
                false,
            ));
        }
        Ok(Self { client, config })
    }

    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    #[must_use]
    pub fn client(&self) -> &RpcClient<C> {
        &self.client
    }

    async fn block_count(&self) -> Result<BlockHeight, SourceError> {
        self.client
            .request_result("getblockcount", serde_json::json!([]))
            .await?
            .deserialize::<u64>()
            .map(BlockHeight)
            .map_err(|error| source_error(error.to_string(), true))
    }

    async fn optional_hash_at(
        &self,
        height: BlockHeight,
    ) -> Result<Option<BlockHash>, SourceError> {
        let raw = self
            .client
            .request_optional_result("getblockhash", serde_json::json!([height.0]), &[-8])
            .await?;
        raw.map(|raw| {
            let hash: String = raw
                .deserialize()
                .map_err(|error| source_error(error.to_string(), true))?;
            parse_bitcoin_block_hash(&hash)
        })
        .transpose()
    }

    async fn hash_at(&self, height: BlockHeight) -> Result<BlockHash, SourceError> {
        self.optional_hash_at(height).await?.ok_or_else(|| {
            source_error(
                "Bitcoin canonical height changed while its hash was being read",
                true,
            )
        })
    }

    async fn raw_block(&self, hash: &BlockHash) -> Result<Option<Vec<u8>>, SourceError> {
        self.client
            .request_optional_result(
                "getblock",
                serde_json::json!([format_bitcoin_block_hash(hash)?, 2]),
                &[-5],
            )
            .await
            .map(|raw| raw.map(|raw| raw.into_bytes()))
    }

    async fn previous_transaction(
        &self,
        transaction_id: TransactionId,
    ) -> Result<bitcoin::Transaction, SourceError> {
        let raw = self
            .client
            .request_optional_result(
                "getrawtransaction",
                serde_json::json!([transaction_id.to_string(), true]),
                &[-5],
            )
            .await?
            .ok_or_else(|| {
                source_error(
                    format!(
                        "Bitcoin previous transaction {transaction_id} disappeared \
                         during block ingestion"
                    ),
                    true,
                )
            })?;
        let value: Value = raw
            .deserialize()
            .map_err(|error| source_error(error.to_string(), true))?;
        let object = value.as_object().ok_or_else(|| {
            source_error("Bitcoin getrawtransaction result must be an object", true)
        })?;
        let returned_id = required_string(object, "txid", "Bitcoin previous transaction ID")?
            .parse::<TransactionId>()
            .map_err(|_| source_error("Bitcoin previous transaction ID is invalid", true))?;
        if returned_id != transaction_id {
            return Err(source_error(
                "Bitcoin previous-transaction lookup returned a different transaction ID",
                true,
            ));
        }
        let transaction = decode_consensus_transaction(object, transaction_id)?;
        let confirmed_block_hash = required_string(
            object,
            "blockhash",
            "Bitcoin previous transaction block hash",
        )?;
        parse_bitcoin_block_hash(confirmed_block_hash)?;

        Ok(transaction)
    }

    async fn resolve_outputs(
        &self,
        transaction_id: TransactionId,
        required_outputs: &BTreeSet<u32>,
    ) -> Result<BTreeMap<u32, ResolvedOutput>, SourceError> {
        let transaction = self.previous_transaction(transaction_id).await?;
        required_outputs
            .iter()
            .map(|output_index| {
                ResolvedOutput::from_transaction(
                    &transaction,
                    transaction_id,
                    *output_index,
                    self.config.network,
                )
                .map(|output| (*output_index, output))
            })
            .collect()
    }

    async fn enrich_prevouts(&self, raw_block: Vec<u8>) -> Result<Vec<u8>, SourceError> {
        let mut value: Value = serde_json::from_slice(&raw_block)
            .map_err(|_| source_error("Bitcoin block result is not valid JSON", true))?;
        let transactions = Transactions::from_block(&value)?;
        let external_outputs = transactions.required_prevouts()?;

        let mut resolved = ResolvedPrevouts::default();
        let requests =
            external_outputs
                .into_iter()
                .map(|(transaction_id, output_indexes)| async move {
                    let data = self
                        .resolve_outputs(transaction_id, &output_indexes)
                        .await?;
                    Ok::<_, SourceError>((transaction_id, data))
                });
        let mut requests = stream::iter(requests).buffer_unordered(MAX_IN_FLIGHT_PREVOUT_REQUESTS);
        while let Some(result) = requests.next().await {
            let (transaction_id, data) = result?;
            resolved.insert(transaction_id, data);
        }

        resolved.enrich_block(&mut value, &transactions)?;

        serde_json::to_vec(&value)
            .map_err(|_| source_error("Bitcoin enriched block JSON could not be encoded", true))
    }

    async fn fetch_block(&self, height: BlockHeight) -> Result<Block, SourceError> {
        let first_hash = self.hash_at(height).await?;
        let Some(raw_block) = self.raw_block(&first_hash).await? else {
            return Err(source_error(
                "Bitcoin Core no longer exposes the requested block",
                true,
            ));
        };
        let raw_block = self.enrich_prevouts(raw_block).await?;
        let block = Block::parse(
            &raw_block,
            Some(height),
            Some(&first_hash),
            self.config.network,
        )
        .map_err(|error| source_error(error.to_string(), true))?;
        drop(raw_block);
        let second_hash = self.hash_at(height).await?;
        if first_hash != second_hash {
            return Err(source_error(
                "Bitcoin canonical block changed while it was being fetched",
                true,
            ));
        }
        Ok(block)
    }
}

mod prevout;

use prevout::*;
impl<C> BlockSource for Blocks<C>
where
    C: Client,
{
    type Block = Block;

    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>> {
        Box::pin(async move {
            let height = self.block_count().await?;
            let hash = self.hash_at(height).await?;
            self.client.header(&hash, height).await
        })
    }

    fn blocks<'a>(
        &'a self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Self::Block>, SourceError>> {
        Box::pin(async move {
            if limit == 0 || start > end {
                return Err(source_error(
                    "Bitcoin block range requires ordered positions and a positive limit",
                    false,
                ));
            }
            let mut position = start.0;
            let end = end.0;
            let mut blocks = Vec::with_capacity(limit.min(64));
            while position <= end && blocks.len() < limit {
                let block = self.fetch_block(BlockHeight(position)).await?;
                blocks.push(block);
                let Some(next) = position.checked_add(1) else {
                    break;
                };
                position = next;
            }
            Ok(blocks)
        })
    }

    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, SourceError>> {
        Box::pin(async move {
            let height = BlockHeight(position.0);
            if height > self.block_count().await? {
                return Ok(None);
            }
            let Some(hash) = self.optional_hash_at(height).await? else {
                return Ok(None);
            };
            self.client.header(&hash, height).await.map(Some)
        })
    }
}

#[cfg(test)]
mod test {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use bitcoin::{
        Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness, absolute,
        consensus, hashes::Hash, hex::DisplayHex, transaction::Version,
    };
    use futures_executor::block_on;
    use json_rpc::{Call, Error, Failure, RawJson};
    use serde_json::{Value, json};

    use super::*;
    use crate::indexer::Outpoint;

    #[derive(Clone)]
    struct ScriptedClient {
        replies: Arc<Mutex<VecDeque<ExpectedReply>>>,
    }

    struct ExpectedReply {
        method: &'static str,
        params: Option<Value>,
        result: Result<Value, i64>,
    }

    impl ScriptedClient {
        fn new(replies: Vec<ExpectedReply>) -> Self {
            Self {
                replies: Arc::new(Mutex::new(replies.into())),
            }
        }

        fn assert_exhausted(&self) {
            assert!(
                self.replies
                    .lock()
                    .expect("script lock must be healthy")
                    .is_empty(),
                "source made fewer requests than scripted"
            );
        }
    }

    impl Client for ScriptedClient {
        fn request<'a>(
            &'a self,
            method: &'a str,
            params: Value,
        ) -> BoxFuture<'a, Result<Result<RawJson, Failure>, Error>> {
            let expected = self
                .replies
                .lock()
                .expect("script lock must be healthy")
                .pop_front()
                .expect("source made more requests than scripted");
            assert_eq!(method, expected.method);
            if let Some(expected_params) = expected.params {
                assert_eq!(params, expected_params);
            }
            let result = expected
                .result
                .map(|value| {
                    RawJson::from_serializable(&value).expect("scripted source result must encode")
                })
                .map_err(|code| Failure {
                    code,
                    message: "scripted source failure".to_owned(),
                    data: None,
                });
            Box::pin(async move { Ok(result) })
        }

        fn request_once<'a>(
            &'a self,
            method: &'a str,
            params: Value,
        ) -> BoxFuture<'a, Result<Result<RawJson, Failure>, Error>> {
            self.request(method, params)
        }

        fn batch<'a>(
            &'a self,
            _requests: Vec<Call>,
        ) -> BoxFuture<'a, Result<Vec<Result<RawJson, Failure>>, Error>> {
            let _unused = Failure {
                code: -1,
                message: String::new(),
                data: None,
            };
            Box::pin(async move { Ok(Vec::new()) })
        }
    }

    fn reply(method: &'static str, result: Value) -> ExpectedReply {
        ExpectedReply {
            method,
            params: None,
            result: Ok(result),
        }
    }

    fn reply_for(method: &'static str, params: Value, result: Value) -> ExpectedReply {
        ExpectedReply {
            method,
            params: Some(params),
            result: Ok(result),
        }
    }

    fn failure(method: &'static str, code: i64) -> ExpectedReply {
        ExpectedReply {
            method,
            params: None,
            result: Err(code),
        }
    }

    fn hash(number: u8) -> String {
        let mut bytes = [0_u8; 32];
        bytes[0] = number;
        bitcoin::BlockHash::from_byte_array(bytes).to_string()
    }

    fn block_result() -> Value {
        let transaction = Transaction {
            version: Version::ONE,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(vec![1, 1]),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(5_000_000_000),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        json!({
            "hash": hash(2),
            "height": 10,
            "previousblockhash": hash(3),
            "time": 100,
            "nTx": 1,
            "tx": [{
                "txid": transaction.compute_txid().to_string(),
                "hex": consensus::serialize(&transaction).to_lower_hex_string(),
                "vin": [{"coinbase": "01"}],
                "vout": [{
                    "value": 50.00000000,
                    "n": 0,
                    "scriptPubKey": {"hex": ""}
                }]
            }]
        })
    }

    fn btc_number(satoshis: u64) -> Value {
        let whole = satoshis / 100_000_000;
        let remainder = satoshis % 100_000_000;
        let lexical = if remainder == 0 {
            whole.to_string()
        } else {
            format!("{whole}.{remainder:08}")
                .trim_end_matches('0')
                .to_owned()
        };
        serde_json::from_str(&lexical).expect("test Bitcoin amount must encode")
    }

    fn transaction_result(transaction: &Transaction, inputs: Vec<Value>) -> Value {
        let outputs = transaction
            .output
            .iter()
            .enumerate()
            .map(|(index, output)| {
                json!({
                    "value": btc_number(output.value.to_sat()),
                    "n": index,
                    "scriptPubKey": {
                        "hex": output.script_pubkey.as_bytes().to_lower_hex_string(),
                    },
                })
            })
            .collect::<Vec<_>>();
        json!({
            "txid": transaction.compute_txid().to_string(),
            "hex": consensus::serialize(transaction).to_lower_hex_string(),
            "vin": inputs,
            "vout": outputs,
        })
    }

    fn external_prevout_block() -> (Value, Transaction, Transaction, Transaction) {
        let previous = Transaction {
            version: Version::ONE,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::from_bytes(vec![1, 1]),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![
                TxOut {
                    value: Amount::from_sat(123_456_789),
                    script_pubkey: ScriptBuf::from_bytes(
                        [vec![0x00, 0x14], vec![0x11; 20]].concat(),
                    ),
                },
                TxOut {
                    value: Amount::from_sat(50_000),
                    script_pubkey: ScriptBuf::from_bytes(
                        [vec![0x51, 0x20], vec![0x22; 32]].concat(),
                    ),
                },
                TxOut {
                    value: Amount::from_sat(25_000),
                    // This historical script is deliberately large. The
                    // source must derive the absence of a canonical address
                    // and discard the script before returning the parsed block.
                    script_pubkey: ScriptBuf::from_bytes(vec![0x61; 65_536]),
                },
            ],
        };
        let spending = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![
                TxIn {
                    previous_output: OutPoint {
                        txid: previous.compute_txid(),
                        vout: 0,
                    },
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: Witness::new(),
                },
                TxIn {
                    previous_output: OutPoint {
                        txid: previous.compute_txid(),
                        vout: 1,
                    },
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: Witness::new(),
                },
                TxIn {
                    previous_output: OutPoint {
                        txid: previous.compute_txid(),
                        vout: 2,
                    },
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: Witness::new(),
                },
            ],
            output: vec![TxOut {
                value: Amount::from_sat(123_521_789),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        let child = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: spending.compute_txid(),
                    vout: 0,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(123_511_789),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        let block = json!({
            "hash": hash(2),
            "height": 10,
            "previousblockhash": hash(3),
            "time": 100,
            "nTx": 2,
            "tx": [
                transaction_result(
                    &spending,
                    vec![
                        json!({"txid": previous.compute_txid().to_string(), "vout": 0}),
                        json!({"txid": previous.compute_txid().to_string(), "vout": 1}),
                        json!({"txid": previous.compute_txid().to_string(), "vout": 2}),
                    ],
                ),
                transaction_result(
                    &child,
                    vec![json!({"txid": spending.compute_txid().to_string(), "vout": 0})],
                ),
            ],
        });
        (block, previous, spending, child)
    }

    fn config() -> Config {
        Config {
            scope: IndexScope {
                chain: ChainId(crate::CHAIN.to_owned()),
                network: "regtest".to_owned(),
            },
            network: Network::Regtest,
            expected_genesis_hash: Some(
                parse_bitcoin_block_hash(&hash(1)).expect("test genesis hash must parse"),
            ),
        }
    }

    fn connect_replies() -> Vec<ExpectedReply> {
        vec![
            reply("getnetworkinfo", json!({"version": 310000})),
            reply(
                "getblockchaininfo",
                json!({
                    "chain": "regtest",
                    "blocks": 10,
                    "headers": 10,
                    "bestblockhash": hash(2),
                    "initialblockdownload": false,
                    "pruned": false
                }),
            ),
            reply(
                "getindexinfo",
                json!({"txindex": {"synced": true, "best_block_height": 10}}),
            ),
            reply("getblockhash", Value::String(hash(1))),
        ]
    }

    mod cases {
        use super::*;
        use crate::{Satoshi, indexer::model::PreviousOutput};

        #[test]
        fn block_consensus_decoding_rejects_invalid_rpc_claims_before_projection() {
            let valid = block_result();
            let trailing = format!("{}00", valid["tx"][0]["hex"].as_str().expect("fixture hex"));
            for (field, value, message) in [
                (
                    "hex",
                    Value::Null,
                    "Bitcoin raw transaction is missing or invalid",
                ),
                (
                    "hex",
                    json!(""),
                    "Bitcoin raw transaction is missing or invalid",
                ),
                (
                    "hex",
                    json!("not-hex"),
                    "Bitcoin transaction hex is invalid",
                ),
                (
                    "hex",
                    json!("ff"),
                    "Bitcoin transaction consensus bytes are invalid",
                ),
                (
                    "hex",
                    json!(trailing),
                    "Bitcoin transaction consensus bytes are invalid",
                ),
                (
                    "txid",
                    json!(TransactionId([0; 32]).to_string()),
                    "Bitcoin transaction ID does not match its consensus bytes",
                ),
            ] {
                let mut block = valid.clone();
                block["tx"][0][field] = value;
                let mut replies = connect_replies();
                replies.extend([
                    reply("getblockhash", Value::String(hash(2))),
                    reply_for("getblock", json!([hash(2), 2]), block),
                ]);
                let client = ScriptedClient::new(replies);
                let calls = client.clone();
                let source =
                    block_on(Blocks::connect(client, config())).expect("valid source setup");
                let error = block_on(source.blocks(BlockPosition(10), BlockPosition(10), 1))
                    .expect_err("invalid consensus claim must stop ingestion");
                assert_eq!(error.message, message);
                assert!(error.retryable);
                calls.assert_exhausted();
            }
        }

        #[test]
        fn input_claim_validation_precedes_prevout_requests() {
            let (valid, _, _, _) = external_prevout_block();
            let mut wrong_outpoint = valid["tx"][0]["vin"].clone();
            wrong_outpoint[0]["vout"] = json!(1);
            let mut invalid_id = valid["tx"][0]["vin"].clone();
            invalid_id[0]["txid"] = json!("invalid");
            invalid_id[0]["vout"] = Value::Null;
            for (inputs, message) in [
                (Value::Null, "Bitcoin transaction inputs must be an array"),
                (
                    json!([]),
                    "Bitcoin transaction input count does not match its consensus bytes",
                ),
                (
                    json!([null, null, null]),
                    "Bitcoin transaction input must be an object",
                ),
                (
                    invalid_id,
                    "Bitcoin input previous transaction ID is invalid",
                ),
                (
                    wrong_outpoint,
                    "Bitcoin input 0 outpoint does not match its consensus bytes",
                ),
            ] {
                let mut block = valid.clone();
                block["tx"][0]["vin"] = inputs;
                let mut replies = connect_replies();
                replies.extend([
                    reply("getblockhash", Value::String(hash(2))),
                    reply_for("getblock", json!([hash(2), 2]), block),
                ]);
                let client = ScriptedClient::new(replies);
                let calls = client.clone();
                let source = block_on(Blocks::connect(client, config())).unwrap();
                let error = block_on(source.blocks(BlockPosition(10), BlockPosition(10), 1))
                    .expect_err(
                        "input claims must be verified before any previous transaction request",
                    );
                assert_eq!(error.message, message);
                assert!(error.retryable);
                calls.assert_exhausted();
            }
        }

        #[test]
        fn numbered_block_fetch_parses_transactions_and_rechecks_canonical_hash() {
            let mut replies = connect_replies();
            replies.extend([
                reply("getblockhash", Value::String(hash(2))),
                reply_for("getblock", json!([hash(2), 2]), block_result()),
                reply("getblockhash", Value::String(hash(2))),
            ]);
            let source = block_on(Blocks::connect(ScriptedClient::new(replies), config()))
                .expect("valid scripted source must connect");

            let block = block_on(source.blocks(BlockPosition(10), BlockPosition(10), 1))
                .expect("canonical verbosity-2 block must load")
                .pop()
                .expect("dense range contains its block");

            assert_eq!(block.reference.height, BlockHeight(10));
            assert_eq!(block.reference.position, BlockPosition(10));
            assert_eq!(
                parse_bitcoin_block_hash(&hash(2)).expect("test hash must parse"),
                block.reference.hash
            );
            assert_eq!(
                block.reference.parent,
                Some(indexing::BlockParent {
                    position: BlockPosition(9),
                    hash: parse_bitcoin_block_hash(&hash(3)).expect("test parent hash must parse"),
                })
            );
            let zero_limit = block_on(source.blocks(BlockPosition(10), BlockPosition(10), 0))
                .expect_err("zero returned-block limit must fail before RPC");
            assert!(!zero_limit.retryable);
            let transactions = block.transactions();
            assert_eq!(transactions.len(), 1);
            assert!(transactions[0].coinbase);
            assert_eq!(transactions[0].inputs.len(), 1);
            assert!(transactions[0].inputs[0].previous_output.is_none());
            assert_eq!(transactions[0].outputs.len(), 1);
            assert_eq!(transactions[0].outputs[0].value, Satoshi(5_000_000_000));
            assert!(transactions[0].outputs[0].script_pubkey.is_empty());
        }

        #[test]
        fn external_prevouts_are_resolved_once_into_bounded_parsed_facts() {
            let (block_result, previous, spending, child) = external_prevout_block();
            let previous_id = previous.compute_txid().to_string();
            let mut replies = connect_replies();
            replies.extend([
                reply("getblockhash", Value::String(hash(2))),
                reply_for("getblock", json!([hash(2), 2]), block_result),
                reply_for(
                    "getrawtransaction",
                    json!([previous_id, true]),
                    json!({
                        "txid": previous.compute_txid().to_string(),
                        "hex": consensus::serialize(&previous).to_lower_hex_string(),
                        "blockhash": hash(5),
                    }),
                ),
                reply("getblockhash", Value::String(hash(2))),
            ]);
            let client = ScriptedClient::new(replies);
            let calls = client.clone();
            let source = block_on(Blocks::connect(client, config()))
                .expect("valid scripted source must connect");

            let block = block_on(source.blocks(BlockPosition(10), BlockPosition(10), 1))
                .expect("external previous outputs must be enriched")
                .pop()
                .expect("dense range contains its block");
            calls.assert_exhausted();

            let transactions = block.transactions();
            assert_eq!(transactions.len(), 2);
            assert_eq!(
                transactions
                    .iter()
                    .map(|transaction| transaction.id)
                    .collect::<Vec<_>>(),
                vec![
                    TransactionId::from(spending.compute_txid()),
                    TransactionId::from(child.compute_txid()),
                ]
            );
            let historical_id = TransactionId::from(previous.compute_txid());
            let expected = previous
                .output
                .iter()
                .enumerate()
                .map(|(index, output)| {
                    Some(PreviousOutput {
                        outpoint: Outpoint {
                            transaction_id: historical_id,
                            output_index: u32::try_from(index).expect("test index must fit u32"),
                        },
                        value: Satoshi(output.value.to_sat()),
                        address: crate::Address::from_script_for_network(
                            &output.script_pubkey,
                            Network::Regtest,
                        ),
                    })
                })
                .collect::<Vec<_>>();
            assert_eq!(
                transactions[0]
                    .inputs
                    .iter()
                    .map(|input| input.previous_output.clone())
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(previous.output[2].script_pubkey.len() > MAX_COMPACT_PREVOUT_JSON_BYTES);

            for output in &previous.output {
                let compact = ResolvedOutput {
                    value_satoshis: output.value.to_sat(),
                    address: crate::Address::from_script_for_network(
                        &output.script_pubkey,
                        Network::Regtest,
                    ),
                }
                .compact_json()
                .expect("resolved output must fit its compact boundary");
                assert_eq!(
                    compact
                        .as_object()
                        .expect("compact previous output must be an object")
                        .len(),
                    2
                );
                assert!(
                    serde_json::to_vec(&compact)
                        .expect("compact data must encode")
                        .len()
                        <= MAX_COMPACT_PREVOUT_JSON_BYTES
                );
            }

            assert_eq!(
                transactions[1].inputs[0].previous_output,
                Some(PreviousOutput {
                    outpoint: Outpoint {
                        transaction_id: TransactionId::from(spending.compute_txid()),
                        output_index: 0,
                    },
                    value: Satoshi(123_521_789),
                    address: None,
                })
            );
        }

        #[test]
        fn in_block_forward_reference_fails_before_external_prevout_requests() {
            let (mut block, _, _, _) = external_prevout_block();
            block["tx"].as_array_mut().unwrap().swap(0, 1);
            let mut replies = connect_replies();
            replies.extend([
                reply("getblockhash", Value::String(hash(2))),
                reply_for("getblock", json!([hash(2), 2]), block),
            ]);
            let client = ScriptedClient::new(replies);
            let calls = client.clone();
            let source = block_on(Blocks::connect(client, config())).expect("valid source setup");

            let error = block_on(source.blocks(BlockPosition(10), BlockPosition(10), 1))
                .expect_err("a child cannot spend a later transaction's output");

            assert_eq!(
                error.message,
                concat!(
                    "Bitcoin block transaction spends an output ",
                    "that was not created earlier in the block"
                )
            );
            assert!(error.retryable);
            calls.assert_exhausted();
        }

        #[test]
        fn enrichment_replaces_external_prevouts_and_removes_in_block_claims() {
            let (mut block, previous, _, _) = external_prevout_block();
            block["tx"][0]["vin"][0]["prevout"] = json!({"untrusted": true});
            block["tx"][1]["vin"][0]["prevout"] = json!({"untrusted": true});
            let mut replies = connect_replies();
            replies.push(reply_for(
                "getrawtransaction",
                json!([previous.compute_txid().to_string(), true]),
                json!({
                    "txid": previous.compute_txid().to_string(),
                    "hex": consensus::serialize(&previous).to_lower_hex_string(),
                    "blockhash": hash(5),
                }),
            ));
            let client = ScriptedClient::new(replies);
            let calls = client.clone();
            let source = block_on(Blocks::connect(client, config())).expect("valid source setup");

            let enriched = block_on(source.enrich_prevouts(serde_json::to_vec(&block).unwrap()))
                .expect("validated external outputs must replace raw RPC claims");
            let enriched: Value = serde_json::from_slice(&enriched).unwrap();
            let address = crate::Address::from_script_for_network(
                &previous.output[0].script_pubkey,
                Network::Regtest,
            )
            .unwrap();
            assert_eq!(
                enriched["tx"][0]["vin"][0]["prevout"],
                json!({"value_satoshis": 123_456_789, "address": address.encoded()})
            );
            assert!(enriched["tx"][1]["vin"][0].get("prevout").is_none());
            calls.assert_exhausted();
        }

        #[test]
        fn external_prevout_lookup_must_return_confirmed_transaction_data() {
            let (block_result, previous, _, _) = external_prevout_block();
            let mut replies = connect_replies();
            replies.extend([
                reply("getblockhash", Value::String(hash(2))),
                reply_for("getblock", json!([hash(2), 2]), block_result),
                reply_for(
                    "getrawtransaction",
                    json!([previous.compute_txid().to_string(), true]),
                    json!({
                        "txid": previous.compute_txid().to_string(),
                        "hex": consensus::serialize(&previous).to_lower_hex_string(),
                    }),
                ),
            ]);
            let client = ScriptedClient::new(replies);
            let calls = client.clone();
            let source = block_on(Blocks::connect(client, config()))
                .expect("valid scripted source must connect");

            let error = block_on(source.blocks(BlockPosition(10), BlockPosition(10), 1))
                .expect_err("mempool-only previous-output data must retry");
            calls.assert_exhausted();
            assert!(error.retryable);
            assert!(error.message.contains("block hash"));
        }

        #[test]
        fn external_prevout_bound_is_above_consensus_maximum_and_fails_before_growth() {
            const CONSENSUS_MAX_BLOCK_WEIGHT: usize = 4_000_000;
            const MINIMUM_INPUT_WEIGHT: usize = 41 * 4;

            let consensus_upper_bound = CONSENSUS_MAX_BLOCK_WEIGHT / MINIMUM_INPUT_WEIGHT;
            assert_eq!(consensus_upper_bound, 24_390);
            assert!(MAX_EXTERNAL_PREVOUTS_PER_BLOCK > consensus_upper_bound);
            assert_eq!(MAX_COMPACT_PREVOUT_TOTAL_BYTES, 4_800_000);

            let transaction_id = TransactionId([0x33; 32]);
            let mut outputs = RequiredPrevouts::default();
            for output_index in 0..MAX_EXTERNAL_PREVOUTS_PER_BLOCK {
                outputs
                    .record(
                        transaction_id,
                        u32::try_from(output_index).expect("test output index must fit u32"),
                    )
                    .expect("consensus-complete safety window must remain accepted");
            }
            let duplicate = outputs
                .record(transaction_id, 0)
                .expect_err("duplicate validation must precede the full-capacity check");
            assert_eq!(
                duplicate.message,
                "Bitcoin block spends the same external outpoint more than once"
            );
            assert!(!duplicate.retryable);
            assert_eq!(outputs.len(), MAX_EXTERNAL_PREVOUTS_PER_BLOCK);
            let error = outputs
                .record(
                    transaction_id,
                    u32::try_from(MAX_EXTERNAL_PREVOUTS_PER_BLOCK)
                        .expect("test output index must fit u32"),
                )
                .expect_err("the first out-of-bound prevout must fail before insertion");
            assert!(!error.retryable);
            assert_eq!(outputs.len(), MAX_EXTERNAL_PREVOUTS_PER_BLOCK);
            let outputs = outputs.into_iter().collect::<BTreeMap<_, _>>();
            assert_eq!(
                outputs
                    .get(&transaction_id)
                    .expect("bounded transaction entry must exist")
                    .len(),
                MAX_EXTERNAL_PREVOUTS_PER_BLOCK
            );
        }

        #[test]
        fn missing_block_fails_before_the_canonical_hash_recheck() {
            let mut replies = connect_replies();
            replies.extend([
                reply_for("getblockhash", json!([10]), Value::String(hash(2))),
                failure("getblock", -5),
            ]);
            let client = ScriptedClient::new(replies);
            let calls = client.clone();
            let source = block_on(Blocks::connect(client, config())).expect("valid source setup");

            let error = block_on(source.blocks(BlockPosition(10), BlockPosition(10), 1))
                .expect_err("a disappeared block must stop ingestion before rechecking its hash");

            assert_eq!(
                error.message,
                "Bitcoin Core no longer exposes the requested block"
            );
            assert!(error.retryable);
            calls.assert_exhausted();
        }

        #[test]
        fn numbered_block_fetch_rejects_same_height_reorg_race() {
            let mut replies = connect_replies();
            replies.extend([
                reply("getblockhash", Value::String(hash(2))),
                reply_for("getblock", json!([hash(2), 2]), block_result()),
                reply("getblockhash", Value::String(hash(4))),
            ]);
            let source = block_on(Blocks::connect(ScriptedClient::new(replies), config()))
                .expect("valid scripted source must connect");

            let error = block_on(source.blocks(BlockPosition(10), BlockPosition(10), 1))
                .expect_err("same-height canonical replacement must retry");

            assert!(error.retryable);
            assert!(error.message.contains("changed"));
        }

        #[test]
        fn disappearing_position_is_optional_for_canonical_reference_and_retryable_for_tip() {
            let mut canonical_replies = connect_replies();
            canonical_replies.extend([
                reply("getblockcount", json!(10)),
                failure("getblockhash", -8),
            ]);
            let canonical = block_on(Blocks::connect(
                ScriptedClient::new(canonical_replies),
                config(),
            ))
            .expect("valid scripted source must connect");
            assert_eq!(
                block_on(canonical.canonical_at(BlockPosition(10)))
                    .expect("a vanished reorg height is not a fatal source error"),
                None
            );

            let mut tip_replies = connect_replies();
            tip_replies.extend([
                reply("getblockcount", json!(10)),
                failure("getblockhash", -8),
            ]);
            let tip = block_on(Blocks::connect(ScriptedClient::new(tip_replies), config()))
                .expect("valid scripted source must connect");
            let error = block_on(tip.tip()).expect_err("tip height race must retry");
            assert!(error.retryable);
        }

        #[test]
        fn canonical_header_keeps_request_identity_and_complete_coordinates() {
            for height in [0, 10] {
                let mut replies = connect_replies();
                replies.extend([
                    reply_for("getblockcount", json!([]), json!(10)),
                    reply_for("getblockhash", json!([height]), json!(hash(2))),
                    reply_for(
                        "getblockheader",
                        json!([hash(2), true]),
                        json!({
                            "height": height,
                            "hash": hash(2).to_uppercase(),
                            "previousblockhash": hash(3),
                            "time": 100,
                        }),
                    ),
                ]);
                let client = ScriptedClient::new(replies);
                let calls = client.clone();
                let source = block_on(Blocks::connect(client, config())).unwrap();
                let header = block_on(source.canonical_at(BlockPosition(height)))
                    .unwrap()
                    .unwrap();
                assert_eq!(header.position, BlockPosition(height));
                assert_eq!(header.height, BlockHeight(height));
                assert_eq!(header.hash, parse_bitcoin_block_hash(&hash(2)).unwrap());
                assert_eq!(header.timestamp, Some(100));
                if height == 0 {
                    assert_eq!(header.parent, None);
                } else {
                    let parent = header.parent.unwrap();
                    assert_eq!(parent.position, BlockPosition(height - 1));
                    assert_eq!(parent.hash, parse_bitcoin_block_hash(&hash(3)).unwrap());
                }
                calls.assert_exhausted();
            }
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
                    "Bitcoin RPC returned an invalid block hash",
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
                    json!({
                        "height": 10,
                        "hash": hash(4),
                        "previousblockhash": hash(3),
                        "time": 100
                    }),
                    "Bitcoin header lookup returned a different block hash",
                ),
            ] {
                let mut replies = connect_replies();
                replies.extend([
                    reply_for("getblockcount", json!([]), json!(10)),
                    reply_for("getblockhash", json!([10]), json!(hash(2))),
                    reply_for("getblockheader", json!([hash(2), true]), header),
                ]);
                let client = ScriptedClient::new(replies);
                let calls = client.clone();
                let source = block_on(Blocks::connect(client, config())).unwrap();
                let error = block_on(source.tip()).unwrap_err();
                assert_eq!(error.message, message);
                assert!(error.retryable);
                calls.assert_exhausted();
            }
        }
    }

    #[test]
    fn previous_transaction_identity_validation_precedes_confirmation_and_selection() {
        let (_, previous, _, _) = external_prevout_block();
        let transaction_id = TransactionId::from(previous.compute_txid());
        let hex = consensus::serialize(&previous).to_lower_hex_string();
        for (result, message) in [
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
                json!({"txid": transaction_id.to_string(), "hex": "ff"}),
                "Bitcoin transaction consensus bytes are invalid",
            ),
            (
                json!({"txid": transaction_id.to_string(), "hex": hex, "blockhash": "bad"}),
                "Bitcoin RPC returned an invalid block hash",
            ),
        ] {
            let mut replies = connect_replies();
            replies.push(reply_for(
                "getrawtransaction",
                json!([transaction_id.to_string(), true]),
                result,
            ));
            let client = ScriptedClient::new(replies);
            let source = block_on(Blocks::connect(client.clone(), config())).unwrap();
            let error =
                block_on(source.resolve_outputs(transaction_id, &BTreeSet::from([u32::MAX])))
                    .unwrap_err();
            assert_eq!(error.message, message);
            assert!(error.retryable);
            client.assert_exhausted();
        }
    }

    #[test]
    fn previous_transaction_resolution_keeps_only_requested_outputs() {
        let (_, previous, _, _) = external_prevout_block();
        let transaction_id = TransactionId::from(previous.compute_txid());
        let mut replies = connect_replies();
        replies.push(reply_for(
            "getrawtransaction",
            json!([transaction_id.to_string(), true]),
            json!({
                "txid": transaction_id.to_string(),
                "hex": consensus::serialize(&previous).to_lower_hex_string(),
                "blockhash": hash(5),
            }),
        ));
        let client = ScriptedClient::new(replies);
        let source = block_on(Blocks::connect(client.clone(), config())).unwrap();
        let outputs =
            block_on(source.resolve_outputs(transaction_id, &BTreeSet::from([1]))).unwrap();
        assert_eq!(outputs.keys().copied().collect::<Vec<_>>(), vec![1]);
        assert_eq!(
            outputs[&1].value_satoshis,
            previous.output[1].value.to_sat()
        );
        assert_eq!(
            outputs[&1].address,
            crate::Address::from_script_for_network(
                &previous.output[1].script_pubkey,
                Network::Regtest,
            )
        );
        client.assert_exhausted();
    }

    #[test]
    fn duplicate_transaction_claims_are_validated_before_duplicate_detection_or_lookups() {
        let (mut block, _, _, _) = external_prevout_block();
        let first = block["tx"][0].clone();
        block["tx"] = json!([first.clone(), first]);
        for invalid_inputs in [true, false] {
            let mut value = block.clone();
            if invalid_inputs {
                value["tx"][1]["vin"] = Value::Null;
            }
            let client = ScriptedClient::new(connect_replies());
            let source = block_on(Blocks::connect(client.clone(), config())).unwrap();
            let error =
                block_on(source.enrich_prevouts(serde_json::to_vec(&value).unwrap())).unwrap_err();
            assert_eq!(
                error.message,
                if invalid_inputs {
                    "Bitcoin transaction inputs must be an array"
                } else {
                    "Bitcoin block contains duplicate transaction IDs"
                }
            );
            assert!(error.retryable);
            client.assert_exhausted();
        }
    }
}
