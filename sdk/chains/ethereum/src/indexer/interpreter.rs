use std::{collections::BTreeSet, sync::LazyLock};

use alloy_primitives::{U256, hex};
use base::Decimal;
use indexing::{
    AssetId, BlockInterpreter as IndexBlockInterpreter, CanonicalAddress, ChainId, IndexError,
    IndexErrorKind, IndexScope, InterpretedBlock, MovementId, NetworkFee, ObservationDraft,
    ObservationDraftStatus, OutputChanges, TransactionRef, ValueMovement,
};
use num_bigint::BigUint;

use super::{
    Block,
    model::{ParseError, ParsedBlock, ParsedLog, ParsedReceipt, ParsedTransaction},
};

const TRANSFER_TOPIC: [u8; 32] = [
    0xdd, 0xf2, 0x52, 0xad, 0x1b, 0xe2, 0xc8, 0x9b, 0x69, 0xc2, 0xb0, 0x68, 0xfc, 0x37, 0x8d, 0xaa,
    0x95, 0x2b, 0xa7, 0xf1, 0x63, 0xc4, 0xa1, 0x16, 0x28, 0xf5, 0x5a, 0x4d, 0xf5, 0x23, 0xb3, 0xef,
];
const ZERO_ADDRESS: [u8; 20] = [0; 20];
static CHAIN_ID: LazyLock<ChainId> = LazyLock::new(|| ChainId(crate::CHAIN.to_owned()));
static NATIVE_ASSET: LazyLock<AssetId> = LazyLock::new(|| AssetId {
    chain: (*CHAIN_ID).clone(),
    asset: "native".to_owned(),
});

struct Movements(Vec<ValueMovement>);

impl Movements {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn successful(
        transaction: &ParsedTransaction,
        receipt: &ParsedReceipt,
        scope: &IndexScope,
    ) -> Result<Self, IndexError> {
        let transaction_id = hex::encode_prefixed(transaction.hash);
        let mut movements = Self::new();
        if !transaction.value.is_zero() {
            let to = transaction.to.or(receipt.contract_address).ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "successful value-bearing Ethereum transaction has no recipient",
                    false,
                )
            })?;
            movements.0.push(ValueMovement::Transfer {
                id: MovementId(format!("{transaction_id}:value")),
                asset: (*NATIVE_ASSET).clone(),
                amount: atomic_decimal(transaction.value),
                from: transaction.from.canonical(scope),
                to: to.canonical(scope),
            });
        }

        movements.0.extend(
            receipt
                .logs
                .iter()
                .filter_map(|log| log.movement(&transaction_id, scope)),
        );
        Ok(movements)
    }

    fn into_vec(self) -> Vec<ValueMovement> {
        self.0
    }
}

impl ParsedLog {
    fn movement(&self, transaction_id: &str, scope: &IndexScope) -> Option<ValueMovement> {
        let [signature, from, to] = self.topics.as_slice() else {
            return None;
        };
        if *signature != TRANSFER_TOPIC
            || self.data.len() != 32
            || from[..12] != [0; 12]
            || to[..12] != [0; 12]
        {
            return None;
        }
        let from: [u8; 20] = from[12..].try_into().ok()?;
        let to: [u8; 20] = to[12..].try_into().ok()?;
        let amount = U256::from_be_slice(&self.data);
        let id = MovementId(format!("{transaction_id}:{}", self.log_index));
        let asset = AssetId {
            chain: (*CHAIN_ID).clone(),
            asset: hex::encode_prefixed(self.address),
        };
        if from == ZERO_ADDRESS {
            (to != ZERO_ADDRESS).then(|| ValueMovement::Mint {
                id,
                asset,
                amount: atomic_decimal(amount),
                to: to.canonical(scope),
            })
        } else if to == ZERO_ADDRESS {
            Some(ValueMovement::Burn {
                id,
                asset,
                amount: atomic_decimal(amount),
                from: from.canonical(scope),
            })
        } else {
            Some(ValueMovement::Transfer {
                id,
                asset,
                amount: atomic_decimal(amount),
                from: from.canonical(scope),
                to: to.canonical(scope),
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockInterpreter {
    scope: IndexScope,
}

impl BlockInterpreter {
    pub fn new(scope: IndexScope) -> Result<Self, IndexError> {
        if scope.chain != *CHAIN_ID {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "Ethereum interpreter scope must use the ethereum chain ID",
                false,
            ));
        }
        if scope.network.trim().is_empty() {
            return Err(IndexError::new(
                IndexErrorKind::ScopeMismatch,
                "Ethereum interpreter network slug must not be empty",
                false,
            ));
        }
        Ok(Self { scope })
    }

    #[must_use]
    pub fn scope(&self) -> &IndexScope {
        &self.scope
    }

    fn matches_address(
        &self,
        transaction: &ParsedTransaction,
        receipt: &ParsedReceipt,
        movements: &Movements,
        addresses: &[CanonicalAddress],
    ) -> Result<bool, IndexError> {
        let mut touched_addresses = BTreeSet::from([transaction.from]);
        if let Some(to) = transaction.to {
            touched_addresses.insert(to);
        }
        if let Some(contract) = receipt.contract_address {
            touched_addresses.insert(contract);
        }
        for movement in &movements.0 {
            for endpoint in [movement.from(), movement.to()].into_iter().flatten() {
                let bytes = parse_canonical_address(endpoint)?;
                touched_addresses.insert(bytes);
            }
        }

        for address in addresses {
            if !address.belongs_to(&self.scope) {
                return Err(IndexError::new(
                    IndexErrorKind::ScopeMismatch,
                    "Ethereum address filter belongs to a different scope",
                    false,
                ));
            }
            if touched_addresses.contains(&parse_canonical_address(address)?) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl ParsedReceipt {
    fn fee(&self, payer: [u8; 20], scope: &IndexScope) -> Result<NetworkFee, IndexError> {
        let fee_amount = self
            .effective_gas_price
            .checked_mul(U256::from(self.gas_used))
            .ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "Ethereum receipt fee exceeds 256-bit atomic units",
                    false,
                )
            })?;
        Ok(NetworkFee {
            asset: (*NATIVE_ASSET).clone(),
            amount: atomic_decimal(fee_amount),
            payer: Some(payer.canonical(scope)),
        })
    }
}

impl IndexBlockInterpreter for BlockInterpreter {
    type Block = Block;

    fn inspect(
        &self,
        block: &Self::Block,
        addresses: &[CanonicalAddress],
    ) -> Result<InterpretedBlock, IndexError> {
        let parsed = ParsedBlock::parse(&block.raw_block, Some(block.reference.height), true)
            .map_err(ParseError::into_invalid_block)?;
        if parsed.reference != block.reference {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "retained Ethereum block reference does not match its raw payload",
                false,
            ));
        }
        let receipts = ParsedReceipt::parse_all(&block.raw_receipts, &parsed)
            .map_err(ParseError::into_invalid_block)?;
        let mut transactions = Vec::new();
        for (transaction, receipt) in parsed.transactions.iter().zip(&receipts) {
            let fee = receipt.fee(transaction.from, &self.scope)?;
            let movements = if receipt.succeeded {
                Movements::successful(transaction, receipt, &self.scope)?
            } else {
                Movements::new()
            };
            if !self.matches_address(transaction, receipt, &movements, addresses)? {
                continue;
            }

            let status = if receipt.succeeded {
                ObservationDraftStatus::Included
            } else {
                ObservationDraftStatus::Failed {
                    reason: Some("ethereum receipt status is zero".to_owned()),
                }
            };
            transactions.push(ObservationDraft {
                scope: self.scope.clone(),
                transaction_id: transaction.hash.canonical(&self.scope),
                status,
                movements: movements.into_vec(),
                fee: Some(fee),
            });
        }

        Ok(InterpretedBlock {
            block: block.reference.clone(),
            transactions,
            outputs: OutputChanges::default(),
        })
    }
}

// Ethereum indexing adapter validates foreign canonical-address chain identity and preserves Alloy
// address syntax with contextual IndexError
// design-lint: allow unclassified-free-function -- See the boundary rationale above.
fn parse_canonical_address(address: &CanonicalAddress) -> Result<[u8; 20], IndexError> {
    if address.scope.chain != *CHAIN_ID {
        return Err(IndexError::new(
            IndexErrorKind::InvalidBlock,
            "Ethereum movement contains a foreign-chain address",
            false,
        ));
    }
    address
        .value
        .parse::<alloy_primitives::Address>()
        .map(alloy_primitives::Address::into_array)
        .map_err(|_| {
            IndexError::new(
                IndexErrorKind::InvalidBlock,
                "Ethereum movement contains a malformed canonical address",
                false,
            )
        })
}

// lossless Ethereum indexer boundary conversion from Alloy U256 to shared scale-zero Decimal for
// native amounts, token units, and fees
// design-lint: allow unclassified-free-function -- See the boundary rationale above.
fn atomic_decimal(value: U256) -> Decimal {
    Decimal::from_atomic(BigUint::from_bytes_be(&value.to_be_bytes::<32>()), 0)
}

trait Canonicalize {
    type Output;

    fn canonical(self, scope: &IndexScope) -> Self::Output;
}

impl Canonicalize for [u8; 20] {
    type Output = CanonicalAddress;

    fn canonical(self, scope: &IndexScope) -> Self::Output {
        CanonicalAddress {
            scope: scope.clone(),
            value: hex::encode_prefixed(self),
        }
    }
}

impl Canonicalize for [u8; 32] {
    type Output = TransactionRef;

    fn canonical(self, scope: &IndexScope) -> Self::Output {
        TransactionRef {
            scope: scope.clone(),
            value: hex::encode_prefixed(self),
        }
    }
}

impl ParseError {
    fn into_invalid_block(self) -> IndexError {
        IndexError::new(IndexErrorKind::InvalidBlock, self.to_string(), false)
    }
}

#[cfg(test)]
mod fee_tests {
    use super::*;
    use alloy_primitives::{Address, B256};
    use alloy_rpc_types_eth::{Block as RpcBlock, BlockTransactions};
    use serde_json::{Value, json};

    #[test]
    fn fee_overflow_precedes_address_filters_for_successful_and_failed_receipts() {
        let scope = IndexScope {
            chain: (*CHAIN_ID).clone(),
            network: "test".to_owned(),
        };
        let interpreter = BlockInterpreter::new(scope.clone()).unwrap();
        let native = RpcBlock::<Value> {
            transactions: BlockTransactions::Full(vec![json!({
                "hash": B256::from([0xcc; 32]).to_string(),
                "from": Address::from([0x11; 20]).to_string(),
                "to": Address::from([0x22; 20]).to_string(),
                "value": "0x0", "transactionIndex": "0x0",
                "blockHash": B256::ZERO.to_string(), "blockNumber": "0x0",
            })]),
            ..Default::default()
        };
        let raw_block = serde_json::to_vec(&native).unwrap();
        let parsed = ParsedBlock::parse(&raw_block, None, true).unwrap();
        let mut receipt = json!({
            "transactionHash": B256::from([0xcc; 32]).to_string(),
            "transactionIndex": "0x0", "blockHash": B256::ZERO.to_string(),
            "blockNumber": "0x0", "from": Address::from([0x11; 20]).to_string(),
            "to": Address::from([0x22; 20]).to_string(), "contractAddress": null,
            "gasUsed": "0x2", "effectiveGasPrice": format!("0x{:x}", U256::MAX), "logs": [],
        });
        let malformed = CanonicalAddress {
            scope: scope.clone(),
            value: "invalid".to_owned(),
        };
        let mut foreign = malformed.clone();
        foreign.scope.network = "foreign".to_owned();
        for status in ["0x0", "0x1"] {
            receipt["status"] = json!(status);
            let block = Block {
                reference: parsed.reference.clone(),
                raw_block: raw_block.clone(),
                raw_receipts: vec![serde_json::to_vec(&receipt).unwrap()],
            };
            for filters in [vec![], vec![malformed.clone()], vec![foreign.clone()]] {
                let error = interpreter.inspect(&block, &filters).unwrap_err();
                assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
                assert_eq!(
                    error.message,
                    "Ethereum receipt fee exceeds 256-bit atomic units"
                );
                assert!(!error.retryable);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use indexing::{
        BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, ChainId, IndexedBlock,
        MovementKind,
    };
    use serde_json::{Value, json};

    use super::*;

    const BLOCK_HASH: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PARENT_HASH: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const TX_HASH: &str = "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const FROM: &str = "0x1111111111111111111111111111111111111111";
    const TO: &str = "0x2222222222222222222222222222222222222222";
    const CONTRACT: &str = "0x3333333333333333333333333333333333333333";
    const TOKEN: &str = "0x4444444444444444444444444444444444444444";

    fn scope() -> IndexScope {
        IndexScope {
            chain: ChainId(crate::CHAIN.to_owned()),
            network: "test".to_owned(),
        }
    }

    fn hash(value: &str) -> [u8; 32] {
        value
            .parse::<alloy_primitives::B256>()
            .expect("test hash must be valid")
            .into()
    }

    fn address(value: &str) -> [u8; 20] {
        value
            .parse::<alloy_primitives::Address>()
            .expect("test address must be valid")
            .into_array()
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

    fn transaction(to: Option<&str>, value: &str) -> Value {
        json!({
            "hash": TX_HASH,
            "from": FROM,
            "to": to,
            "value": value,
            "transactionIndex": "0x0",
            "blockHash": BLOCK_HASH,
            "blockNumber": "0xa"
        })
    }

    fn receipt(
        succeeded: bool,
        to: Option<&str>,
        contract: Option<&str>,
        gas_price: &str,
        logs: Vec<Value>,
    ) -> Value {
        json!({
            "transactionHash": TX_HASH,
            "transactionIndex": "0x0",
            "blockHash": BLOCK_HASH,
            "blockNumber": "0xa",
            "from": FROM,
            "to": to,
            "contractAddress": contract,
            "status": if succeeded { "0x1" } else { "0x0" },
            "gasUsed": "0x5208",
            "effectiveGasPrice": gas_price,
            "logs": logs
        })
    }

    fn log(index: u64, from: &str, to: &str, data: &str) -> Value {
        let topic_address = |address: &str| format!("0x{}{}", "00".repeat(12), &address[2..]);
        json!({
            "address": TOKEN,
            "topics": [
                hex::encode_prefixed(TRANSFER_TOPIC),
                topic_address(from),
                topic_address(to)
            ],
            "data": data,
            "blockHash": BLOCK_HASH,
            "blockNumber": "0xa",
            "transactionHash": TX_HASH,
            "transactionIndex": "0x0",
            "logIndex": format!("0x{index:x}"),
            "removed": false
        })
    }

    fn ethereum_block(transaction: Value, receipt: Value) -> Block {
        let raw_block =
            serde_json::to_vec(&block_value(transaction)).expect("test block JSON must serialize");
        let parsed = ParsedBlock::parse(&raw_block, Some(BlockHeight(10)), true)
            .expect("test block must parse");
        Block {
            reference: parsed.reference,
            raw_block,
            raw_receipts: vec![
                serde_json::to_vec(&receipt).expect("test receipt JSON must serialize"),
            ],
        }
    }

    fn canonical_address(value: &str) -> CanonicalAddress {
        address(value).canonical(&scope())
    }

    fn inspect(
        block: &Block,
        addresses: &[CanonicalAddress],
    ) -> Result<InterpretedBlock, IndexError> {
        BlockInterpreter::new(scope())?.inspect(block, addresses)
    }

    #[test]
    fn interprets_successful_native_transfer_and_actual_fee() {
        let block = ethereum_block(
            transaction(Some(TO), "0x2a"),
            receipt(true, Some(TO), None, "0x3", Vec::new()),
        );
        let interpreted = inspect(&block, &[canonical_address(TO)]).expect("block must interpret");
        let draft = interpreted
            .transactions
            .first()
            .expect("relevant tx must emit");
        assert_eq!(draft.movements.len(), 1);
        assert_eq!(
            draft.movements[0].id(),
            &MovementId(format!("{TX_HASH}:value"))
        );
        assert_eq!(draft.movements[0].amount(), &Decimal::from(42_u64));
        assert_eq!(
            draft.fee.as_ref().expect("fee must exist").amount,
            Decimal::from(63_000_u64)
        );
        assert_eq!(draft.movements[0].amount().scale(), 0);
        assert_eq!(
            draft.fee.as_ref().expect("fee must exist").amount.scale(),
            0
        );
        assert_eq!(draft.status, ObservationDraftStatus::Included);
    }

    #[test]
    fn preserves_full_width_atomic_native_token_and_fee_amounts() {
        let maximum = format!("0x{}", "ff".repeat(32));
        let zero = "0x0000000000000000000000000000000000000000";
        let block = ethereum_block(
            transaction(Some(TO), &maximum),
            receipt(
                true,
                Some(TO),
                None,
                &format!("0x1{}", "0".repeat(60)),
                vec![
                    log(0, FROM, TO, &maximum),
                    log(1, zero, TO, &maximum),
                    log(2, FROM, zero, &maximum),
                ],
            ),
        );
        let interpreted = inspect(&block, &[canonical_address(FROM)])
            .expect("full-width amounts with a non-overflowing fee must interpret");
        let draft = &interpreted.transactions[0];
        let expected =
            "115792089237316195423570985008687907853269984665640564039457584007913129639935"
                .parse::<Decimal>()
                .expect("maximum U256 decimal literal must parse");
        assert_eq!(
            draft
                .movements
                .iter()
                .map(ValueMovement::kind)
                .collect::<Vec<_>>(),
            [
                MovementKind::Transfer,
                MovementKind::Transfer,
                MovementKind::Mint,
                MovementKind::Burn
            ]
        );
        for movement in &draft.movements {
            assert_eq!(movement.amount(), &expected);
            assert_eq!(movement.amount().scale(), 0);
        }

        // The fixture charges 21,000 gas at 2^240 atomic units per gas.
        let expected_fee =
            "37103788360346070921249247515601288832377161834387998120553730227145015296000"
                .parse::<Decimal>()
                .expect("full-width fee decimal literal must parse");
        let fee = draft.fee.as_ref().expect("fee must exist");
        assert_eq!(fee.amount, expected_fee);
        assert_eq!(fee.amount.scale(), 0);
    }

    #[test]
    fn sends_contract_creation_value_to_receipt_contract() {
        let block = ethereum_block(
            transaction(None, "0x7"),
            receipt(true, None, Some(CONTRACT), "0x1", Vec::new()),
        );
        let interpreted = inspect(&block, &[canonical_address(CONTRACT)])
            .expect("contract creation must interpret");
        assert_eq!(
            interpreted.transactions[0].movements[0].to(),
            Some(&address(CONTRACT).canonical(&scope()))
        );
    }

    #[test]
    fn failed_receipt_is_fee_only() {
        let block = ethereum_block(
            transaction(Some(TO), "0x2a"),
            receipt(false, Some(TO), None, "0x2", Vec::new()),
        );
        let interpreted =
            inspect(&block, &[canonical_address(FROM)]).expect("failure must interpret");
        let draft = &interpreted.transactions[0];
        assert!(draft.movements.is_empty());
        assert!(matches!(
            draft.status,
            ObservationDraftStatus::Failed { .. }
        ));
        assert!(draft.fee.is_some());
    }

    #[test]
    fn ignores_transactions_unrelated_to_the_address_filter() {
        let block = ethereum_block(
            transaction(Some(TO), "0x2a"),
            receipt(true, Some(TO), None, "0x2", Vec::new()),
        );

        let interpreted = inspect(&block, &[canonical_address(CONTRACT)])
            .expect("unrelated transaction must still interpret");

        assert!(interpreted.transactions.is_empty());
    }

    #[test]
    fn rejects_an_address_filter_from_another_scope() {
        let block = ethereum_block(
            transaction(Some(TO), "0x2a"),
            receipt(true, Some(TO), None, "0x2", Vec::new()),
        );
        let mut foreign = canonical_address(TO);
        foreign.scope.network = "other".to_owned();

        let error = inspect(&block, &[foreign]).expect_err("foreign scope must be rejected");

        assert_eq!(error.kind, IndexErrorKind::ScopeMismatch);
    }

    #[test]
    fn rejects_fee_multiplication_overflow() {
        let block = ethereum_block(
            transaction(Some(TO), "0x0"),
            receipt(
                true,
                Some(TO),
                None,
                "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                Vec::new(),
            ),
        );
        let error = inspect(&block, &[canonical_address(FROM)])
            .expect_err("overflowing actual fee must fail the block");
        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
        assert!(error.message.contains("fee exceeds"));
    }

    #[test]
    fn interprets_transfer_mint_and_burn_logs() {
        let zero = "0x0000000000000000000000000000000000000000";
        let logs = vec![
            log(0, FROM, TO, &format!("0x{:064x}", 1)),
            log(1, zero, TO, &format!("0x{:064x}", 2)),
            log(2, FROM, zero, &format!("0x{:064x}", 3)),
        ];
        let block = ethereum_block(
            transaction(Some(TO), "0x0"),
            receipt(true, Some(TO), None, "0x1", logs),
        );
        let interpreted = inspect(&block, &[canonical_address(FROM)]).expect("logs must interpret");
        let movements = &interpreted.transactions[0].movements;
        assert_eq!(movements.len(), 3);
        assert_eq!(movements[0].kind(), MovementKind::Transfer);
        assert_eq!(movements[1].kind(), MovementKind::Mint);
        assert_eq!(movements[1].from(), None);
        assert_eq!(movements[2].kind(), MovementKind::Burn);
        assert_eq!(movements[2].to(), None);
        assert_eq!(movements[2].id(), &MovementId(format!("{TX_HASH}:2")));
        for (movement, amount) in movements.iter().zip([1_u64, 2, 3]) {
            assert_eq!(movement.amount(), &Decimal::from(amount));
            assert_eq!(movement.amount().scale(), 0);
        }
    }

    #[test]
    fn ignores_structurally_malformed_transfer_log() {
        let mut malformed = log(0, FROM, TO, &format!("0x{:064x}", 1));
        malformed["topics"] = json!([hex::encode_prefixed(TRANSFER_TOPIC)]);
        let block = ethereum_block(
            transaction(Some(TO), "0x0"),
            receipt(true, Some(TO), None, "0x1", vec![malformed]),
        );
        let interpreted = inspect(&block, &[canonical_address(FROM)])
            .expect("malformed token log must not poison the block");
        assert!(interpreted.transactions[0].movements.is_empty());
    }

    #[test]
    fn rejects_receipt_transaction_mismatch() {
        let mut wrong_receipt = receipt(true, Some(TO), None, "0x1", Vec::new());
        wrong_receipt["transactionHash"] = Value::String(
            "0xdddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_owned(),
        );
        let block = ethereum_block(transaction(Some(TO), "0x0"), wrong_receipt);
        let error = inspect(&block, &[canonical_address(FROM)])
            .expect_err("receipt mismatch must fail the block");
        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
        assert!(error.message.contains("transaction hash"));
    }

    #[test]
    fn raw_block_reference_is_stable() {
        let block = ethereum_block(
            transaction(Some(TO), "0x0"),
            receipt(true, Some(TO), None, "0x1", Vec::new()),
        );
        assert_eq!(
            block.block_ref(),
            BlockRef {
                position: BlockPosition(10),
                height: BlockHeight(10),
                hash: BlockHash(hash(BLOCK_HASH).to_vec()),
                parent: Some(BlockParent {
                    position: BlockPosition(9),
                    hash: BlockHash(hash(PARENT_HASH).to_vec()),
                }),
                timestamp: Some(100),
            }
        );
    }

    #[test]
    fn canonical_identities_preserve_scope_leading_zeroes_and_lowercase_hex() {
        let scope = scope();
        let mut address = [0; 20];
        address[18..].copy_from_slice(&[0xab, 0xff]);
        let canonical = address.canonical(&scope);
        assert_eq!(canonical.scope, scope);
        assert_eq!(canonical.value, format!("0x{}abff", "00".repeat(18)));

        let hash: [u8; 32] = std::array::from_fn(|index| index as u8);
        let transaction = hash.canonical(&scope);
        assert_eq!(transaction.scope, scope);
        assert_eq!(
            transaction.value,
            "0x000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
        );
    }

    #[test]
    fn malformed_block_and_receipt_errors_remain_terminal_before_projection() {
        let mut block = ethereum_block(
            transaction(Some(TO), "0x2a"),
            receipt(true, Some(TO), None, "0x3", Vec::new()),
        );
        block.raw_receipts[0] = b"not-json".to_vec();
        let error =
            inspect(&block, &[]).expect_err("invalid receipt must fail even without filters");
        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
        assert_eq!(error.message, "Ethereum receipt result is not valid JSON");
        assert!(!error.retryable);

        block.raw_block = b"not-json".to_vec();
        let error =
            inspect(&block, &[]).expect_err("invalid block must fail before receipt parsing");
        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
        assert_eq!(
            error.message,
            "Ethereum block result does not match the RPC block shape"
        );
        assert!(!error.retryable);
    }

    #[test]
    fn canonical_address_adapter_preserves_chain_precedence_and_alloy_syntax() {
        for prefix in ["", "0x", "0X"] {
            let address = CanonicalAddress {
                scope: scope(),
                value: format!("{prefix}{}", "aB".repeat(20)),
            };
            assert_eq!(parse_canonical_address(&address).unwrap(), [0xab; 20]);
        }
        let mut address = CanonicalAddress {
            scope: scope(),
            value: "invalid".into(),
        };
        let error = parse_canonical_address(&address).unwrap_err();
        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
        assert!(!error.retryable);
        assert_eq!(
            error.message,
            "Ethereum movement contains a malformed canonical address"
        );
        address.scope.chain = ChainId("foreign".into());
        let error = parse_canonical_address(&address).unwrap_err();
        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
        assert!(!error.retryable);
        assert_eq!(
            error.message,
            "Ethereum movement contains a foreign-chain address"
        );
    }
}
