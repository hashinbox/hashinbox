//! Blocks as retained evidence ([`RawBlock`]) and as parsed facts ([`Block`]).
//!
//! `RawBlock` → [`RawBlock::parse`] → [`Block`] → [`Transaction`] →
//! [`Transaction::movements`] → [`Movement`]. Parsing happens only here.

mod field;
mod head;
mod log;
mod receipt;
mod trace;
mod transaction;
mod withdrawal;

use base::BlockRef;

pub(crate) use head::Head;
pub use log::Log;
pub use receipt::Receipt;
pub use trace::{Balance, Trace};
pub use transaction::{Fee, Movement, Transaction};
pub use withdrawal::Withdrawal;

use crate::native::Error;

/// One parsed block: its reference and transactions in block order.
pub type Block = base::Block<Transaction>;
/// Transactions of one block in block order, with unique ids.
pub type Transactions = base::Transactions<Transaction>;

/// Exact source evidence for one block, retained for replay and reorg recovery.
///
/// The payload is the JSON-RPC `result` bytes of `eth_getBlockByNumber` (with
/// full transactions), of each transaction receipt in block order, and of the
/// block's `debug_traceBlockByHash` prestate diff. Keeping bytes rather than
/// parsed facts makes replay independent of this parser.
///
/// Traces are an order of magnitude larger than the block that carries them
/// and are held for one indexing pass only: nothing writes a `RawBlock` to
/// storage, and no raw-block archive exists to write one to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawBlock {
    reference: BlockRef,
    block: Vec<u8>,
    receipts: Vec<Vec<u8>>,
    traces: Vec<u8>,
}

impl RawBlock {
    /// Pairs evidence with the reference it claims; [`Self::parse`] checks both agree.
    #[must_use]
    pub const fn new(
        reference: BlockRef,
        block: Vec<u8>,
        receipts: Vec<Vec<u8>>,
        traces: Vec<u8>,
    ) -> Self {
        Self {
            reference,
            block,
            receipts,
            traces,
        }
    }

    /// The block this evidence claims to be.
    #[must_use]
    pub const fn reference(&self) -> &BlockRef {
        &self.reference
    }

    /// The `eth_getBlockByNumber` result bytes.
    #[must_use]
    pub fn block(&self) -> &[u8] {
        &self.block
    }

    /// One receipt result per transaction, in block order.
    #[must_use]
    pub fn receipts(&self) -> &[Vec<u8>] {
        &self.receipts
    }

    /// The `debug_traceBlockByHash` result bytes: one prestate diff per
    /// transaction, in block order.
    #[must_use]
    pub fn traces(&self) -> &[u8] {
        &self.traces
    }

    /// Every consensus-layer credit this block makes, in block order.
    ///
    /// These are the block's value changes that **no transaction names**, so
    /// they are not in [`Self::parse`]'s result and cannot be: a [`Block`] is
    /// its transactions. An account that only ever received withdrawals has
    /// an empty history and a non-zero balance, which is why this is a
    /// balance fact rather than a history nicety (`docs/design/021` section
    /// 4.1).
    ///
    /// It reads the payload already held — no RPC call, and nothing of the
    /// transaction path.
    ///
    /// # Errors
    ///
    /// [`crate::native::ErrorKind::InvalidBlock`] naming the first violation.
    pub fn withdrawals(&self) -> Result<Vec<Withdrawal>, Error> {
        Withdrawal::parse_all(&self.block)
    }

    /// Validates the evidence and decodes it into a [`Block`].
    ///
    /// # Errors
    ///
    /// [`crate::native::ErrorKind::InvalidBlock`] naming the first violation.
    pub fn parse(&self) -> Result<Block, Error> {
        let head = Head::parse(&self.block, Some(self.reference.height), true)?;
        if head.reference != self.reference {
            return Err(Error::invalid_block(
                "retained Ethereum block reference does not match its raw payload",
            ));
        }
        let receipts = Receipt::parse_all(&self.receipts, &head)?;
        let traces = Trace::parse_all(&self.traces, &head)?;
        let transactions = head
            .transactions
            .into_iter()
            .zip(receipts)
            .zip(traces)
            .map(|((sent, receipt), trace)| Transaction::new(sent, receipt, trace))
            .collect::<Result<Vec<_>, _>>()?;
        let transactions = Transactions::new(transactions).map_err(|_| {
            Error::invalid_block("Ethereum block contains duplicate transaction hashes")
        })?;
        Ok(Block::new(head.reference, transactions))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, U256};
    use base::{BlockHash, BlockHeight, BlockParent, BlockPosition, MovementKind};
    use serde_json::{Value, json};

    use super::head::Sent;
    use super::*;
    use crate::native::{Address, AssetKind, TransactionId, Wei, erc20::TRANSFER_TOPIC};

    const BLOCK_HASH: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PARENT_HASH: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const TX_HASH: &str = "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const FROM: &str = "0x1111111111111111111111111111111111111111";
    const TO: &str = "0x2222222222222222222222222222222222222222";
    const TOKEN: &str = "0x4444444444444444444444444444444444444444";
    const BENEFICIARY: &str = "0x5555555555555555555555555555555555555555";

    fn head(hash: Vec<u8>) -> Head {
        Head {
            reference: BlockRef {
                position: BlockPosition(10),
                height: BlockHeight(10),
                hash: BlockHash(hash),
                parent: Some(BlockParent {
                    position: BlockPosition(9),
                    hash: BlockHash(vec![0xbb; 32]),
                }),
                timestamp: Some(100),
            },
            transactions: vec![Sent {
                id: TransactionId::from_bytes([0xcc; 32]),
                from: Address::from_bytes([0x11; 20]),
                to: Some(Address::from_bytes([0x22; 20])),
                value: U256::from(42_u8),
                index: 0,
            }],
        }
    }

    fn receipt_value(sent: &Sent, log_indexes: &[u64]) -> Value {
        let logs = log_indexes
            .iter()
            .map(|index| {
                json!({
                    "blockHash": B256::from([0xaa; 32]).to_string(),
                    "blockNumber": "0xa",
                    "transactionHash": sent.id.to_string(),
                    "transactionIndex": format!("0x{:x}", sent.index),
                    "removed": false,
                    "address": Address::from_bytes([0x33; 20]).to_string(),
                    "topics": [],
                    "data": "0x00abff",
                    "logIndex": format!("0x{index:x}")
                })
            })
            .collect::<Vec<_>>();
        json!({
            "transactionHash": sent.id.to_string(),
            "transactionIndex": format!("0x{:x}", sent.index),
            "blockHash": B256::from([0xaa; 32]).to_string(),
            "blockNumber": "0xa",
            "from": sent.from.to_string(),
            "to": sent.to.as_ref().map(ToString::to_string),
            "contractAddress": null,
            "status": "0x1",
            "gasUsed": "0x5208",
            "effectiveGasPrice": "0x3",
            "logs": logs
        })
    }

    fn receipts(head: &Head, values: &[Value]) -> Result<Vec<Receipt>, Error> {
        let raw = values
            .iter()
            .map(|value| serde_json::to_vec(value).unwrap())
            .collect::<Vec<_>>();
        Receipt::parse_all(&raw, head)
    }

    #[test]
    fn receipt_fields_precede_logs_and_preserve_full_width_execution_values() {
        let mut head = head(vec![0xaa; 32]);
        let mut original = receipt_value(&head.transactions[0], &[]);
        original["gasUsed"] = json!("0xffffffffffffffff");
        original["effectiveGasPrice"] = json!(format!("0x{:x}", U256::MAX));
        let parsed = receipts(&head, &[original.clone()]).unwrap();
        assert_eq!(parsed[0].gas_used, u64::MAX);
        assert_eq!(parsed[0].effective_gas_price, Wei::from_atomic([255; 32]));
        for (field, invalid, message) in [
            (
                "transactionHash",
                json!(B256::ZERO.to_string()),
                "Ethereum receipt transaction hash does not match block order",
            ),
            (
                "transactionIndex",
                json!("0x1"),
                "Ethereum receipt transaction index does not match block order",
            ),
            (
                "blockHash",
                json!(B256::ZERO.to_string()),
                "Ethereum receipt block hash does not match its block",
            ),
            (
                "blockNumber",
                json!("0xb"),
                "Ethereum receipt block number does not match its block",
            ),
            (
                "from",
                json!(Address::ZERO.to_string()),
                "Ethereum receipt sender does not match its transaction",
            ),
            (
                "to",
                Value::Null,
                "Ethereum receipt recipient does not match its transaction",
            ),
            (
                "status",
                json!("0x2"),
                "Ethereum receipt status must be zero or one",
            ),
            (
                "gasUsed",
                json!("0x10000000000000000"),
                "Ethereum receipt gas used exceeds u64",
            ),
            (
                "effectiveGasPrice",
                json!("0x00"),
                "Ethereum receipt effective gas price is not a canonical hex quantity",
            ),
            (
                "contractAddress",
                json!(Address::ZERO.to_string()),
                "non-creation receipt unexpectedly contains a contract address",
            ),
        ] {
            let mut value = original.clone();
            value["logs"] = Value::Null;
            value[field] = invalid;
            assert_eq!(receipts(&head, &[value]).unwrap_err().message, message);
        }
        head.transactions[0].to = None;
        original["to"] = Value::Null;
        original["logs"] = Value::Null;
        assert_eq!(
            receipts(&head, &[original]).unwrap_err().message,
            "successful value-bearing contract creation has no contract address"
        );
    }

    #[test]
    fn log_identity_and_payload_errors_are_terminal() {
        let head = head(vec![0xaa; 32]);
        let original = receipt_value(&head.transactions[0], &[3]);
        let mut invalid = original.clone();
        invalid["logs"][0]["blockHash"] = json!(B256::ZERO.to_string());
        invalid["logs"][0]
            .as_object_mut()
            .unwrap()
            .remove("blockNumber");
        let mut removed = original.clone();
        removed["logs"][0]["removed"] = json!(true);
        removed["logs"][0]["address"] = Value::Null;
        let mut topics = original.clone();
        topics["logs"][0]["topics"] = json!([null]);
        topics["logs"][0]["data"] = Value::Null;
        let mut shape = original;
        shape["logs"][0] = json!(7);
        for (value, message) in [
            (
                invalid,
                "Ethereum log identity does not match its receipt and block",
            ),
            (removed, "canonical Ethereum receipt contains a removed log"),
            (topics, "Ethereum log topic must be a hash"),
            (shape, "Ethereum receipt log must be an object"),
        ] {
            assert_eq!(receipts(&head, &[value]).unwrap_err().message, message);
        }
    }

    #[test]
    fn receipt_log_order_and_uniqueness_are_block_wide() {
        let mut head = head(vec![0xaa; 32]);
        let mut second = head.transactions[0].clone();
        second.id = TransactionId::from_bytes([0xdd; 32]);
        second.index = 1;
        head.transactions.push(second);
        let first = receipt_value(&head.transactions[0], &[0, 4]);
        let parsed = receipts(
            &head,
            &[first.clone(), receipt_value(&head.transactions[1], &[7, 9])],
        )
        .unwrap();
        assert_eq!(
            parsed
                .iter()
                .flat_map(|receipt| &receipt.logs)
                .map(|log| (log.index, log.data.as_slice()))
                .collect::<Vec<_>>(),
            vec![
                (0, &[0, 0xab, 0xff][..]),
                (4, &[0, 0xab, 0xff][..]),
                (7, &[0, 0xab, 0xff][..]),
                (9, &[0, 0xab, 0xff][..]),
            ]
        );
        for (index, expected) in [
            (4, "Ethereum block contains duplicate log indexes"),
            (0, "Ethereum block contains duplicate log indexes"),
            (3, "Ethereum logs are not ordered by log index"),
        ] {
            let second = receipt_value(&head.transactions[1], &[index]);
            let error = receipts(&head, &[first.clone(), second]).unwrap_err();
            assert_eq!(error.message, expected);
        }
    }

    #[test]
    fn receipt_log_validation_stops_at_the_first_invalid_log() {
        let head = head(vec![0xaa; 32]);
        let mut value = receipt_value(&head.transactions[0], &[4, 4, 5]);
        value["logs"][2]["data"] = json!("0xgg");
        assert_eq!(
            receipts(&head, &[value.clone()]).unwrap_err().message,
            "Ethereum block contains duplicate log indexes"
        );
        value["logs"][1]["data"] = json!("0xgg");
        assert_eq!(
            receipts(&head, &[value.clone()]).unwrap_err().message,
            "Ethereum log data is not valid hex"
        );
        value["transactionHash"] = json!(B256::from([0xdd; 32]).to_string());
        assert_eq!(
            receipts(&head, &[value]).unwrap_err().message,
            "Ethereum receipt transaction hash does not match block order"
        );
    }

    #[test]
    fn receipt_parsing_checks_count_then_hash_length_then_json() {
        let short = head(vec![0xaa; 31]);
        assert_eq!(
            Receipt::parse_all(&[], &short).unwrap_err().message,
            "Ethereum receipt count does not match transaction count"
        );
        for length in [31, 33] {
            let error =
                Receipt::parse_all(&[b"not-json".to_vec()], &head(vec![0xaa; length])).unwrap_err();
            assert_eq!(error.message, "Ethereum block hash is not 32 bytes");
        }
        let mut reversed = head((0_u8..32).collect());
        let value = json!({
            "transactionHash": TX_HASH, "transactionIndex": "0x0",
            "blockHash": "0x000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            "blockNumber": "0xa", "from": FROM, "to": TO, "contractAddress": null,
            "status": "0x1", "gasUsed": "0x5208", "effectiveGasPrice": "0x3", "logs": []
        });
        let parsed = receipts(&reversed, std::slice::from_ref(&value)).unwrap();
        assert_eq!(
            parsed,
            vec![Receipt {
                succeeded: true,
                gas_used: 21_000,
                effective_gas_price: Wei::from_u128(3),
                contract_address: None,
                logs: Vec::new(),
            }]
        );
        reversed.reference.hash.0.reverse();
        assert_eq!(
            receipts(&reversed, &[value]).unwrap_err().message,
            "Ethereum receipt block hash does not match its block"
        );
        let object = json!(null);
        assert_eq!(
            receipts(&head(vec![0xaa; 32]), &[object])
                .unwrap_err()
                .message,
            "Ethereum receipt must be an object"
        );
    }

    #[test]
    fn log_data_keeps_accepted_bytes_and_ordered_errors() {
        let head = head(vec![0xaa; 32]);
        let base = receipt_value(&head.transactions[0], &[2]);
        for (data, expected) in [
            ("0x", vec![]),
            ("0x00aBfF", vec![0, 0xab, 0xff]),
            // The decoder has always accepted a plus-prefixed pair.
            ("0x+1+a", vec![1, 10]),
        ] {
            let mut value = base.clone();
            value["logs"][0]["data"] = json!(data);
            let parsed = receipts(&head, &[value]).unwrap();
            assert_eq!(parsed[0].logs[0].data, expected, "data: {data}");
            assert_eq!(parsed[0].logs[0].index, 2);
        }
        for (data, message) in [
            (json!(null), "Ethereum log data must be a hex byte string"),
            (json!(17), "Ethereum log data must be a hex byte string"),
            (json!("00"), "Ethereum log data is not hex encoded"),
            (json!("0X00"), "Ethereum log data is not hex encoded"),
            (
                json!("0x0"),
                "Ethereum log data has an odd number of hex digits",
            ),
            (
                json!("0x€"),
                "Ethereum log data has an odd number of hex digits",
            ),
            (json!("0xgg"), "Ethereum log data is not valid hex"),
            (json!("0xé"), "Ethereum log data is not valid hex"),
            (json!("0x€a"), "Ethereum log data is not valid hex"),
        ] {
            let mut value = base.clone();
            value["logs"][0]["data"] = data;
            value["logs"][0].as_object_mut().unwrap().remove("logIndex");
            assert_eq!(receipts(&head, &[value]).unwrap_err().message, message);
        }
        let mut value = base;
        value["logs"][0].as_object_mut().unwrap().remove("logIndex");
        assert_eq!(
            receipts(&head, &[value]).unwrap_err().message,
            "Ethereum log index is missing"
        );
    }

    fn block_value(transaction: Value) -> Value {
        json!({
            "hash": BLOCK_HASH,
            "parentHash": PARENT_HASH,
            "sha3Uncles": "0xdddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            "miner": "0x0000000000000000000000000000000000000000",
            "stateRoot": "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "transactionsRoot":
                "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "receiptsRoot": "0xabababababababababababababababababababababababababababababababab",
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "difficulty": "0x0",
            "number": "0xa",
            "gasLimit": "0x1c9c380",
            "gasUsed": "0x5208",
            "timestamp": "0x64",
            "extraData": "0x",
            "mixHash": "0xcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
            "nonce": "0x0000000000000000",
            "uncles": [],
            "transactions": [transaction]
        })
    }

    fn transfer_log(index: u64, from: &str, to: &str, data: &str) -> Value {
        let topic = |address: &str| format!("0x{}{}", "00".repeat(12), &address[2..]);
        let signature = alloy_primitives::hex::encode_prefixed(TRANSFER_TOPIC);
        json!({
            "address": TOKEN,
            "topics": [signature, topic(from), topic(to)],
            "data": data,
            "blockHash": BLOCK_HASH,
            "blockNumber": "0xa",
            "transactionHash": TX_HASH,
            "transactionIndex": "0x0",
            "logIndex": format!("0x{index:x}"),
            "removed": false
        })
    }

    /// A prestate diff in which the only ether that moved is the top-level
    /// `value` and the gas its sender paid, so nothing is left unexplained and
    /// the transaction yields no residual.
    fn settled_trace(recipient: Option<&str>, value: &str, gas_price: &str) -> Value {
        let quantity = |text: &str| {
            U256::from_str_radix(text.trim_start_matches("0x"), 16).unwrap_or(U256::ZERO)
        };
        let value = quantity(value);
        let spent = value.saturating_add(quantity(gas_price).saturating_mul(U256::from(21_000)));
        let balance = |amount: U256| json!({"balance": format!("{amount:#x}")});
        let mut pre = serde_json::Map::new();
        let mut post = serde_json::Map::new();
        pre.insert(FROM.to_owned(), balance(spent));
        post.insert(FROM.to_owned(), balance(U256::ZERO));
        if let Some(recipient) = recipient.filter(|_| !value.is_zero()) {
            pre.insert(recipient.to_owned(), balance(U256::ZERO));
            post.insert(recipient.to_owned(), balance(value));
        }
        json!([{"txHash": TX_HASH, "result": {"pre": pre, "post": post}}])
    }

    fn raw_block(value: &str, gas_price: &str, logs: Vec<Value>) -> RawBlock {
        let traces = settled_trace(Some(TO), value, gas_price);
        traced_block(value, gas_price, logs, traces)
    }

    fn traced_block(value: &str, gas_price: &str, logs: Vec<Value>, traces: Value) -> RawBlock {
        let transaction = json!({
            "hash": TX_HASH, "from": FROM, "to": TO, "value": value,
            "transactionIndex": "0x0", "blockHash": BLOCK_HASH, "blockNumber": "0xa"
        });
        let receipt = json!({
            "transactionHash": TX_HASH, "transactionIndex": "0x0", "blockHash": BLOCK_HASH,
            "blockNumber": "0xa", "from": FROM, "to": TO, "contractAddress": null,
            "status": "0x1", "gasUsed": "0x5208", "effectiveGasPrice": gas_price, "logs": logs
        });
        let block = serde_json::to_vec(&block_value(transaction)).unwrap();
        let reference = Head::parse(&block, None, true).unwrap().reference;
        RawBlock::new(
            reference,
            block,
            vec![serde_json::to_vec(&receipt).unwrap()],
            serde_json::to_vec(&traces).unwrap(),
        )
    }

    /// One prestate diff entry for this fixture's single transaction.
    fn diff(pre: Value, post: Value) -> Value {
        json!([{"txHash": TX_HASH, "result": {"pre": pre, "post": post}}])
    }

    fn held(amount: u128) -> Value {
        json!({"balance": format!("{amount:#x}")})
    }

    fn movements(block: &Block) -> Vec<(String, MovementKind, Wei)> {
        block
            .transactions()
            .iter()
            .next()
            .expect("the fixture has one transaction")
            .movements()
            .into_iter()
            .map(|movement| (movement.id, movement.kind, movement.amount))
            .collect()
    }

    #[test]
    fn parse_decodes_native_and_token_movements_and_the_fee() {
        let zero = "0x0000000000000000000000000000000000000000";
        let raw = raw_block(
            "0x2a",
            "0x3",
            vec![
                transfer_log(0, FROM, TO, &format!("0x{:064x}", 1)),
                transfer_log(1, zero, TO, &format!("0x{:064x}", 2)),
                transfer_log(2, FROM, zero, &format!("0x{:064x}", 3)),
                transfer_log(3, zero, zero, &format!("0x{:064x}", 4)),
            ],
        );
        let block = raw.parse().expect("fixture must parse");
        assert_eq!(block.reference(), raw.reference());
        let transaction = &block.transactions().iter().next().unwrap();
        assert!(transaction.succeeded());
        let fee = transaction.fee().unwrap();
        assert_eq!(fee.amount, Wei::from_u128(63_000));
        assert_eq!(fee.payer, Some(Address::from_bytes([0x11; 20])));
        let movements = transaction.movements();
        assert_eq!(
            movements
                .iter()
                .map(|movement| (movement.id.as_str(), movement.kind))
                .collect::<Vec<_>>(),
            [
                (format!("{TX_HASH}:value").as_str(), MovementKind::Transfer),
                (format!("{TX_HASH}:0").as_str(), MovementKind::Transfer),
                (format!("{TX_HASH}:1").as_str(), MovementKind::Mint),
                (format!("{TX_HASH}:2").as_str(), MovementKind::Burn),
            ]
        );
        assert_eq!(movements[0].asset, AssetKind::Native);
        assert_eq!(movements[0].amount, Wei::from_u128(42));
        assert_eq!(movements[1].asset, AssetKind::Erc20(TOKEN.parse().unwrap()));
        assert_eq!(
            (movements[2].from.as_ref(), movements[3].to.as_ref()),
            (None, None)
        );
    }

    /// The double-count trap of `docs/design/021` section 6.2: the trace
    /// carries the very ether the top-level `value` already names, so a plain
    /// transfer must still produce exactly one movement — and the fee trap
    /// beside it: traced balances already include the gas the sender paid, so
    /// the sender is left with nothing unexplained and no residual repeats a
    /// charge the `Fee` already records.
    #[test]
    fn a_plain_transfer_is_counted_once_and_its_gas_is_never_charged_twice() {
        // 42 wei of value and 21,000 gas at 3 wei: the sender is out 63,042.
        let raw = traced_block(
            "0x2a",
            "0x3",
            Vec::new(),
            diff(
                json!({FROM: held(63_042), TO: held(0)}),
                json!({FROM: held(0), TO: held(42)}),
            ),
        );
        let block = raw.parse().expect("a settled transfer must parse");
        assert_eq!(
            movements(&block),
            [(
                format!("{TX_HASH}:value"),
                MovementKind::Transfer,
                Wei::from_u128(42)
            )]
        );
        let transaction = block.transactions().iter().next().unwrap();
        assert_eq!(transaction.fee().unwrap().amount, Wei::from_u128(63_000));
        assert_eq!(
            transaction.trace.balance(&Address::from_bytes([0x11; 20])),
            Some(&Balance {
                before: Wei::from_u128(63_042),
                after: Wei::ZERO,
            })
        );
    }

    /// A router that is sent ether and forwards it on: the recipient the
    /// transaction names ends where it started, and the account the contract
    /// paid is named by nothing in the block or its receipts.
    #[test]
    fn contract_initiated_ether_is_attributed_to_the_transaction_that_moved_it() {
        let raw = traced_block(
            "0x2a",
            "0x3",
            Vec::new(),
            diff(
                json!({FROM: held(63_042), BENEFICIARY: held(0)}),
                json!({FROM: held(0), BENEFICIARY: held(42)}),
            ),
        );
        let block = raw.parse().expect("a forwarded transfer must parse");
        assert_eq!(
            movements(&block),
            [
                (
                    format!("{TX_HASH}:value"),
                    MovementKind::Transfer,
                    Wei::from_u128(42)
                ),
                (
                    format!("{TX_HASH}:residual:{TO}"),
                    MovementKind::Burn,
                    Wei::from_u128(42)
                ),
                (
                    format!("{TX_HASH}:residual:{BENEFICIARY}"),
                    MovementKind::Mint,
                    Wei::from_u128(42)
                ),
            ]
        );
    }

    /// A `SELFDESTRUCT`: the contract is absent from the diff's `post` state,
    /// so it holds nothing, and its beneficiary is credited with what it held.
    #[test]
    fn a_selfdestruct_debits_the_contract_and_credits_its_beneficiary() {
        let raw = traced_block(
            "0x0",
            "0x3",
            Vec::new(),
            diff(
                json!({FROM: held(63_000), TOKEN: held(100), BENEFICIARY: held(7)}),
                json!({FROM: held(0), BENEFICIARY: held(107)}),
            ),
        );
        let block = raw.parse().expect("a selfdestruct must parse");
        assert_eq!(
            movements(&block),
            [
                (
                    format!("{TX_HASH}:residual:{TOKEN}"),
                    MovementKind::Burn,
                    Wei::from_u128(100)
                ),
                (
                    format!("{TX_HASH}:residual:{BENEFICIARY}"),
                    MovementKind::Mint,
                    Wei::from_u128(100)
                ),
            ]
        );
    }

    /// An account created by the transaction starts at zero.
    #[test]
    fn a_created_account_starts_at_zero() {
        let created = diff(
            json!({FROM: held(63_000)}),
            json!({FROM: held(0), TO: held(5)}),
        );
        let raw = traced_block("0x0", "0x3", Vec::new(), created);
        assert_eq!(
            movements(&raw.parse().expect("a created account must parse")),
            [(
                format!("{TX_HASH}:residual:{TO}"),
                MovementKind::Mint,
                Wei::from_u128(5)
            )]
        );
    }

    /// A revert undoes the execution, never the fee. The diff of a failed
    /// transaction therefore reports the settlement only — the sender's gas
    /// charge, which the transaction's own [`Fee`] already records and the
    /// residual cancels, and the beneficiary's priority fee, which nothing
    /// else records. Dropping the second understates a watched fee recipient
    /// permanently, because the balance is folded from these movements.
    #[test]
    fn a_failed_transaction_still_credits_the_beneficiary_its_priority_fee() {
        // 21 000 gas at 3 wei: 63 000 charged to the sender, of which the
        // beneficiary earns 21 000 and the rest is burned.
        let settlement = diff(
            json!({FROM: held(63_000), BENEFICIARY: held(0)}),
            json!({FROM: held(0), BENEFICIARY: held(21_000)}),
        );
        let raw = traced_block("0x5", "0x3", Vec::new(), settlement);
        let mut failed = serde_json::from_slice::<Value>(raw.receipts()[0].as_slice()).unwrap();
        failed["status"] = json!("0x0");
        let raw = RawBlock::new(
            raw.reference().clone(),
            raw.block().to_vec(),
            vec![serde_json::to_vec(&failed).unwrap()],
            raw.traces().to_vec(),
        );
        let block = raw.parse().expect("a failed transaction parses");
        let transaction = block
            .transactions()
            .iter()
            .next()
            .expect("the fixture has one transaction");
        assert!(!transaction.succeeded());
        // The 5 wei the transaction carried never moved, and the sender's own
        // balance comes out at the fee the `Fee` record already states.
        assert_eq!(
            movements(&block),
            [(
                format!("{TX_HASH}:residual:{BENEFICIARY}"),
                MovementKind::Mint,
                Wei::from_u128(21_000)
            )]
        );
        assert_eq!(
            transaction.fee().map(|fee| fee.amount.clone()),
            Some(Wei::from_u128(63_000))
        );
    }

    #[test]
    fn trace_evidence_must_describe_this_block_and_be_a_diff() {
        let settled = diff(json!({FROM: held(63_042)}), json!({FROM: held(0)}));
        for (traces, message) in [
            (
                json!("0x"),
                "Ethereum block trace must be an array of transaction traces",
            ),
            (
                json!([]),
                "Ethereum block trace count does not match transaction count",
            ),
            (json!([7]), "Ethereum transaction trace must be an object"),
            (
                json!([{"txHash": TX_HASH, "error": "out of gas"}]),
                "Ethereum node could not trace one transaction of the block",
            ),
            (
                diff(json!({}), json!({})).as_array().unwrap()[0]
                    .as_object()
                    .map(|entry| {
                        let mut entry = entry.clone();
                        entry.insert("txHash".to_owned(), json!(BLOCK_HASH));
                        json!([entry])
                    })
                    .unwrap(),
                "Ethereum transaction trace hash does not match block order",
            ),
            (
                json!([{"txHash": TX_HASH}]),
                "Ethereum transaction trace carries no prestate result",
            ),
            (
                json!([{"txHash": TX_HASH, "result": {"pre": {}}}]),
                "Ethereum transaction trace has no `post` state, so it is not a diff",
            ),
            (
                json!([{"result": {"pre": {"0x11": {}}, "post": {}}}]),
                "Ethereum trace account is not a canonical 20-byte address",
            ),
            (
                json!([{"result": {"pre": {FROM: 7}, "post": {}}}]),
                "Ethereum trace account must be an object",
            ),
            (
                json!([{"result": {"pre": {FROM: {"balance": "0x00"}}, "post": {}}}]),
                "Ethereum trace account balance is not a canonical hex quantity",
            ),
            (
                json!([{"result": {"pre": {FROM: {"balance": 9}}, "post": {}}}]),
                "Ethereum trace account balance must be a hex quantity string",
            ),
            (
                json!([{"result": {"pre": {FROM: {}}, "post": {FROM: held(9)}}}]),
                "Ethereum trace reports a new account balance without its prior balance",
            ),
        ] {
            let raw = traced_block("0x0", "0x3", Vec::new(), traces);
            assert_eq!(raw.parse().unwrap_err().message, message);
        }
        traced_block("0x2a", "0x3", Vec::new(), settled)
            .parse()
            .expect("a trace without the recipient still parses");
    }

    #[test]
    fn parse_rejects_a_mismatched_reference_and_fee_overflow() {
        let raw = raw_block("0x0", "0x1", Vec::new());
        let mut reference = raw.reference().clone();
        reference.timestamp = Some(1);
        let moved = RawBlock::new(
            reference,
            raw.block().to_vec(),
            raw.receipts().to_vec(),
            raw.traces().to_vec(),
        );
        assert_eq!(
            moved.parse().unwrap_err().message,
            "retained Ethereum block reference does not match its raw payload"
        );
        let overflow = raw_block("0x0", &format!("0x{}", "f".repeat(64)), Vec::new());
        let error = overflow.parse().unwrap_err();
        assert_eq!(error.kind, crate::native::ErrorKind::InvalidBlock);
        assert_eq!(
            error.message,
            "Ethereum receipt fee exceeds 256-bit atomic units"
        );
    }

    #[test]
    fn a_block_body_credit_is_read_from_the_payload_the_transactions_came_from() {
        let raw = raw_block("0x0", "0x1", Vec::new());
        // The same bytes, with the body a post-Shanghai node sends.
        let mut body: Value = serde_json::from_slice(raw.block()).unwrap();
        body["withdrawals"] = json!([{
            "index": "0x11", "validatorIndex": "0x2a",
            "address": BENEFICIARY, "amount": "0x77359400",
        }]);
        let shanghai = RawBlock::new(
            raw.reference().clone(),
            serde_json::to_vec(&body).unwrap(),
            raw.receipts().to_vec(),
            raw.traces().to_vec(),
        );
        let credits = shanghai.withdrawals().expect("the body carries a credit");
        assert_eq!(credits.len(), 1);
        assert_eq!(credits[0].id(), "withdrawal:17");
        assert_eq!(credits[0].validator_index, 42);
        assert_eq!(credits[0].address, BENEFICIARY.parse::<Address>().unwrap());
        // 2 * 10^9 Gwei is 2 ether, not 2 Gwei.
        assert_eq!(credits[0].amount.decimal().to_string(), "2");
        // It reaches no transaction: the beneficiary has a balance and no
        // history at all, which is exactly why the fold needs it separately.
        let block = shanghai.parse().expect("the transaction path is untouched");
        assert!(
            block
                .transactions()
                .iter()
                .flat_map(Transaction::movements)
                .all(|movement| movement.to.as_ref()
                    != Some(&BENEFICIARY.parse::<Address>().unwrap()))
        );
        // A pre-Shanghai body has no such key, and that is not a defect.
        assert!(
            raw.withdrawals()
                .expect("no key is an empty list")
                .is_empty()
        );
    }
}
