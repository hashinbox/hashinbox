//! [`indexing::BlockInterpreter`]: parsed native facts → canonical observations.

use std::collections::BTreeSet;

use base::{MovementKind, Networked};
use indexing::{
    BlockCredit, BlockInterpreter as Interpret, IndexError, InterpretedBlock, MovementId, Network,
    NetworkFee, ObservationDraft, ObservationDraftStatus, ValueMovement,
};

use crate::native::{
    Address, AssetKind, CHAIN_NAME,
    block::{Fee, Movement, RawBlock, Transaction, Withdrawal},
};

/// Interprets Ethereum blocks for one `(ethereum, network)` network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockInterpreter {
    network: Network,
}

impl BlockInterpreter {
    /// # Errors
    ///
    /// `NetworkMismatch` unless `network` names an Ethereum network.
    pub fn new(network: &impl Networked) -> Result<Self, IndexError> {
        let network = network.canonical();
        indexing::require_chain(&network, &CHAIN_NAME)?;
        Ok(Self { network })
    }

    /// The one network this interpreter accepts.
    #[must_use]
    pub const fn network(&self) -> &Network {
        &self.network
    }

    fn observation(&self, transaction: &Transaction, movements: Vec<Movement>) -> ObservationDraft {
        let status = if transaction.succeeded() {
            ObservationDraftStatus::Included
        } else {
            ObservationDraftStatus::Failed {
                reason: Some("ethereum receipt status is zero".to_owned()),
            }
        };
        ObservationDraft {
            transaction_id: transaction.id().canonical(&self.network),
            // The node's own `transactionIndex`, checked against the block's
            // transaction list and against the receipt when the block was
            // parsed (`native/block/head.rs`, `native/block/receipt.rs`). Never
            // this loop's counter: `inspect` reports the selected transactions
            // only, so counting them would number the selection.
            transaction_order: transaction.listed.index,
            status,
            movements: movements
                .into_iter()
                .filter_map(|movement| self.movement(movement))
                .collect(),
            fee: transaction.fee().map(|fee| self.fee(fee)),
        }
    }

    /// `None` only for a movement without the endpoints its kind requires.
    fn movement(&self, movement: Movement) -> Option<ValueMovement> {
        let id = MovementId(movement.id);
        let asset = movement.asset.id();
        let amount = movement.amount.atomic();
        let endpoint =
            |address: Option<Address>| address.map(|value| value.canonical(&self.network));
        let (from, to) = (endpoint(movement.from), endpoint(movement.to));
        match (movement.kind, from, to) {
            (MovementKind::Mint, None, Some(to)) => {
                Some(ValueMovement::mint(id, asset, amount, to))
            }
            (MovementKind::Burn, Some(from), None) => {
                Some(ValueMovement::burn(id, asset, amount, from))
            }
            (_, Some(from), Some(to)) => Some(ValueMovement::transfer(id, asset, amount, from, to)),
            _ => None,
        }
    }

    /// One consensus-layer withdrawal as the transactionless credit it is.
    ///
    /// Ether only, and always: a withdrawal is a protocol payment of the
    /// native asset, so nothing here consults an asset kind. The identity is
    /// the consensus layer's own index, which is unique for the life of the
    /// chain, so re-inspecting the block produces the same record and a store
    /// keyed by it cannot hold two.
    fn credit(&self, withdrawal: &Withdrawal) -> BlockCredit {
        BlockCredit {
            id: MovementId(withdrawal.id()),
            asset: AssetKind::Native.id(),
            amount: withdrawal.amount.atomic(),
            to: withdrawal.address.canonical(&self.network),
        }
    }

    fn fee(&self, fee: &Fee) -> NetworkFee {
        NetworkFee {
            asset: AssetKind::Native.id(),
            amount: fee.amount.atomic(),
            payer: fee
                .payer
                .as_ref()
                .map(|payer| payer.canonical(&self.network)),
        }
    }
}

impl Interpret for BlockInterpreter {
    type Block = RawBlock;

    /// Gas is charged outside the transferred value — the traced residual
    /// adds it back on the credit side — so a balance fold debits it from the
    /// payer, in ether only.
    fn settlement(&self) -> indexing::FeeSettlement {
        indexing::FeeSettlement::Charged
    }

    /// The evidence is parsed first, then the address filter is checked —
    /// always, whatever the block turns out to hold. One trait method answers
    /// one question one way on every chain: a block this interpreter cannot
    /// read is an `InvalidBlock` before anything is asked about the filter,
    /// and a filter of another network is a `NetworkMismatch` even for a block
    /// with nothing in it (`docs/DEFECT_PATTERNS.md` section 2.3).
    fn inspect(
        &self,
        block: &RawBlock,
        addresses: &[base::Address],
    ) -> Result<InterpretedBlock, IndexError> {
        let parsed = block.parse()?;
        let selected: BTreeSet<Address> = indexing::select_addresses(&self.network, addresses)?;
        let mut transactions = Vec::new();
        for transaction in parsed.transactions() {
            let movements = transaction.movements();
            let touched = transaction
                .parties()
                .chain(movements.iter().flat_map(Movement::endpoints));
            if touched
                .into_iter()
                .any(|address| selected.contains(address))
            {
                transactions.push(self.observation(transaction, movements));
            }
        }
        let credits = block
            .withdrawals()?
            .iter()
            .filter(|withdrawal| selected.contains(&withdrawal.address))
            .map(|withdrawal| self.credit(withdrawal))
            .collect();
        let reference = block.reference().clone();
        Ok(InterpretedBlock {
            credits,
            ..InterpretedBlock::new(parsed.network(), reference, transactions)?
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{U256, hex};
    use base::Decimal;
    use indexing::{
        BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, IndexErrorKind, NativeBlock,
    };
    use serde_json::{Value, json};

    use super::*;
    use crate::native::{CHAIN_NAME, TransactionId, block::Head, erc20::TRANSFER_TOPIC};

    const BLOCK_HASH: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PARENT_HASH: &str = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const TX_HASH: &str = "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const SECOND_TX_HASH: &str =
        "0x1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c";
    const FROM: &str = "0x1111111111111111111111111111111111111111";
    const TO: &str = "0x2222222222222222222222222222222222222222";
    const CONTRACT: &str = "0x3333333333333333333333333333333333333333";
    const TOKEN: &str = "0x4444444444444444444444444444444444444444";
    const BENEFICIARY: &str = "0x5555555555555555555555555555555555555555";

    fn network() -> Network {
        Network {
            chain: CHAIN_NAME,
            name: "test".to_owned(),
        }
    }

    /// The native network whose canonical identity is [`network`].
    fn native_network() -> crate::native::Network {
        crate::native::Network::new(31_337, "test")
    }

    fn hash(value: &str) -> [u8; 32] {
        value
            .parse::<alloy_primitives::B256>()
            .expect("test hash must be valid")
            .into()
    }

    fn address(value: &str) -> Address {
        value.parse().expect("test address must be valid")
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

    fn ethereum_block(transaction: Value, receipt: Value) -> RawBlock {
        let traces = settled_trace(
            receipt["to"]
                .as_str()
                .or_else(|| receipt["contractAddress"].as_str()),
            transaction["value"].as_str().unwrap_or("0x0"),
            receipt["effectiveGasPrice"].as_str().unwrap_or("0x0"),
        );
        traced_block(transaction, receipt, traces)
    }

    fn traced_block(transaction: Value, receipt: Value, traces: Value) -> RawBlock {
        let raw_block =
            serde_json::to_vec(&block_value(transaction)).expect("test block JSON must serialize");
        let head =
            Head::parse(&raw_block, Some(BlockHeight(10)), true).expect("test block must parse");
        RawBlock::new(
            head.reference,
            native_network(),
            raw_block,
            vec![serde_json::to_vec(&receipt).expect("test receipt JSON must serialize")],
            serde_json::to_vec(&traces).expect("test trace JSON must serialize"),
        )
    }

    /// A block with no transactions and no withdrawals: nothing to interpret,
    /// so nothing forces the address filter to be looked at on its way past.
    fn empty_block() -> RawBlock {
        let mut value = block_value(transaction(Some(TO), "0x0"));
        value["transactions"] = json!([]);
        let raw_block = serde_json::to_vec(&value).expect("test block JSON must serialize");
        let head =
            Head::parse(&raw_block, Some(BlockHeight(10)), true).expect("test block must parse");
        RawBlock::new(
            head.reference,
            native_network(),
            raw_block,
            Vec::new(),
            b"[]".to_vec(),
        )
    }

    fn canonical_address(value: &str) -> base::Address {
        address(value).canonical(&network())
    }

    fn inspect(
        block: &RawBlock,
        addresses: &[base::Address],
    ) -> Result<InterpretedBlock, IndexError> {
        BlockInterpreter::new(&network())?.inspect(block, addresses)
    }

    /// The settled balance of `of` in ether, folded from `interpreted`
    /// exactly as a store folds the rows it wrote from it, under this chain's
    /// own fee rule — both collections, because a store writes both.
    fn folded(interpreted: &InterpretedBlock, of: base::Address) -> indexing::Balance {
        let subject = indexing::Subject {
            address: of,
            asset: AssetKind::Native.id(),
        };
        let settlement = BlockInterpreter::new(&network())
            .expect("the fixture network is Ethereum")
            .settlement();
        let mut ledger = indexing::Fold::new(&subject);
        for draft in interpreted.block.transactions() {
            let status = match &draft.status {
                ObservationDraftStatus::Included => indexing::CanonicalStatus::Included {
                    block: interpreted.block.reference().clone(),
                },
                ObservationDraftStatus::Failed { reason } => indexing::CanonicalStatus::Failed {
                    block: interpreted.block.reference().clone(),
                    reason: reason.clone(),
                },
            };
            ledger
                .apply(
                    &indexing::CanonicalTransaction {
                        transaction_id: draft.transaction_id.clone(),
                        transaction_order: draft.transaction_order,
                        status,
                        movements: draft.movements.clone(),
                        fee: draft.fee.clone(),
                    },
                    settlement,
                )
                .expect("interpreted amounts are storable");
        }
        for credit in &interpreted.credits {
            ledger
                .apply_credit(credit)
                .expect("interpreted amounts are storable");
        }
        ledger.balance(Some(interpreted.block.reference().clone()))
    }

    #[test]
    fn contract_initiated_ether_reaches_the_balance_instead_of_being_absorbed() {
        // A contract holding 5 wei forwards all of it to the watched address;
        // the transaction's own `value` is zero and no log is emitted, so the
        // traced residual is the only record there is. 0xf618 is the gas the
        // sender paid, which is already inside its traced balance.
        let traces = json!([{"txHash": TX_HASH, "result": {
            "pre": {FROM: {"balance": "0xf618"}, CONTRACT: {"balance": "0x5"},
                    TO: {"balance": "0x0"}},
            "post": {FROM: {"balance": "0x0"}, CONTRACT: {"balance": "0x0"},
                     TO: {"balance": "0x5"}}
        }}]);
        let block = traced_block(
            transaction(Some(CONTRACT), "0x0"),
            receipt(true, Some(CONTRACT), None, "0x3", Vec::new()),
            traces,
        );
        let interpreted = inspect(&block, &[canonical_address(TO)]).expect("block must interpret");

        let credited = folded(&interpreted, canonical_address(TO));
        assert_eq!(credited.amount, Decimal::from(5_u64));
        assert_eq!(credited.completeness, indexing::Completeness::Incomplete);
        // The sender is charged its gas once: the fee is outside the movements
        // on this chain, and the residual added it back on the credit side so
        // that `NetworkFee` stays the only record of it.
        let sender = folded(&interpreted, canonical_address(FROM));
        assert_eq!(sender.completeness, indexing::Completeness::Overdrawn);
    }

    /// The block, with `withdrawals` replacing whatever the body carried.
    fn withdrawing_block(plain: &RawBlock, withdrawals: Value) -> RawBlock {
        let mut body: Value =
            serde_json::from_slice(plain.block()).expect("the fixture block is JSON");
        body["withdrawals"] = withdrawals;
        RawBlock::new(
            plain.reference().clone(),
            plain.network().clone(),
            serde_json::to_vec(&body).expect("the fixture block serializes"),
            plain.receipts().to_vec(),
            plain.traces().to_vec(),
        )
    }

    /// The block body credits a watched
    /// address two ether with no transaction, `InterpretedBlock::credits`
    /// carries it, and the fold reports it — beside the 42 wei the block's one
    /// transaction moved to the same address, each counted exactly once.
    ///
    /// The two facts stay separate on purpose: the credit is not a movement of
    /// that transaction, so a caller asking which transaction caused it gets
    /// no answer, because there is none (section 4.4 rejected inventing one).
    #[test]
    fn a_consensus_layer_credit_folds_beside_a_transaction_to_the_same_address() {
        let plain = ethereum_block(
            transaction(Some(TO), "0x2a"),
            receipt(true, Some(TO), None, "0x3", Vec::new()),
        );
        let block = withdrawing_block(
            &plain,
            json!([
                {"index": "0x1", "validatorIndex": "0x2",
                 "address": TO, "amount": "0x77359400"},
                {"index": "0x2", "validatorIndex": "0x3",
                 "address": CONTRACT, "amount": "0x1"},
            ]),
        );

        let read = block.withdrawals().expect("the body carries two credits");
        // 0x77359400 Gwei is two ether, not two Gwei.
        assert_eq!(read[0].amount.decimal().to_string(), "2");
        assert_eq!(
            read[0].amount.atomic(),
            Decimal::from(2_000_000_000_000_000_000_u64)
        );

        let beneficiary = canonical_address(TO);
        let interpreted =
            inspect(&block, std::slice::from_ref(&beneficiary)).expect("block must interpret");

        // The unwatched second beneficiary is not carried, exactly as an
        // unwatched transaction is not.
        assert_eq!(interpreted.credits.len(), 1);
        assert_eq!(interpreted.credits[0].id, MovementId("withdrawal:1".into()));
        assert_eq!(interpreted.credits[0].to, beneficiary);
        assert_eq!(
            interpreted.credits[0].amount,
            Decimal::from(2_000_000_000_000_000_000_u64)
        );
        // The transaction still reports its own value and nothing else: the
        // credit was not folded into it.
        let draft = interpreted
            .block
            .transactions()
            .iter()
            .next()
            .expect("the value transfer");
        assert_eq!(draft.movements.len(), 1);
        assert_eq!(draft.movements[0].amount, Decimal::from(42_u64));

        let credited = folded(&interpreted, beneficiary);
        assert_eq!(
            credited.amount,
            Decimal::from(2_000_000_000_000_000_042_u64),
            "two ether from the block body plus 42 wei from the transaction"
        );
        assert_eq!(credited.completeness, indexing::Completeness::Incomplete);
    }

    /// The case the old code was silently wrong for, and the one that has no
    /// other record at all: an address with no transaction history and a
    /// non-zero balance.
    #[test]
    fn an_address_funded_only_by_withdrawals_has_no_transactions_and_a_balance() {
        let plain = ethereum_block(
            transaction(Some(TO), "0x2a"),
            receipt(true, Some(TO), None, "0x3", Vec::new()),
        );
        let block = withdrawing_block(
            &plain,
            json!([{"index": "0x7", "validatorIndex": "0x2",
                   "address": CONTRACT, "amount": "0x77359400"}]),
        );

        let beneficiary = canonical_address(CONTRACT);
        let interpreted =
            inspect(&block, std::slice::from_ref(&beneficiary)).expect("block must interpret");
        assert!(
            interpreted.block.transactions().is_empty(),
            "no transaction names this credit"
        );
        assert_eq!(interpreted.credits[0].id, MovementId("withdrawal:7".into()));
        let credited = folded(&interpreted, beneficiary);
        assert_eq!(
            credited.amount,
            Decimal::from(2_000_000_000_000_000_000_u64)
        );
    }

    /// A draft's `transaction_order` is its place in the **block**, not its
    /// place in what `inspect` reported.
    ///
    /// The block holds two transactions and only the second touches a selected
    /// address, so an interpreter numbering the drafts it built would answer
    /// `0`. The number is the node's own `transactionIndex`, which the parse
    /// has already checked against block order and against the receipt, so the
    /// same transaction carries the same ordinal under every selection — which
    /// is what keeps a history cursor stable
    /// (`indexing::ObservationDraft::transaction_order`).
    #[test]
    fn a_draft_carries_its_place_in_the_block_not_in_the_selection() {
        let block = two_transaction_block();
        let selected = inspect(&block, &[canonical_address(TO)]).expect("block must interpret");
        assert_eq!(
            selected.block.transactions().len(),
            1,
            "one transaction is selected"
        );
        assert_eq!(selected.block.transactions()[0].transaction_order, 1);
        assert_eq!(
            selected.block.transactions()[0]
                .transaction_id
                .id
                .to_string(),
            SECOND_TX_HASH
        );

        // The wider selection reports both, and the second one keeps the very
        // ordinal it had when it was reported alone.
        let both = inspect(
            &block,
            &[canonical_address(TO), canonical_address(BENEFICIARY)],
        )
        .expect("block must interpret");
        let orders: Vec<u64> = both
            .block
            .transactions()
            .iter()
            .map(|draft| draft.transaction_order)
            .collect();
        assert_eq!(orders, [0, 1]);
    }

    /// One block whose first transaction pays `BENEFICIARY` and whose second
    /// pays `TO`, each with its own receipt and prestate trace.
    fn two_transaction_block() -> RawBlock {
        let placed = |hash: &str, index: u64, to: &str| {
            let mut sent = transaction(Some(to), "0x2a");
            sent["hash"] = hash.into();
            sent["transactionIndex"] = format!("{index:#x}").into();
            let mut paid = receipt(true, Some(to), None, "0x3", Vec::new());
            paid["transactionHash"] = hash.into();
            paid["transactionIndex"] = format!("{index:#x}").into();
            let mut traced = settled_trace(Some(to), "0x2a", "0x3")[0].clone();
            traced["txHash"] = hash.into();
            (sent, paid, traced)
        };
        let (first, first_receipt, first_trace) = placed(TX_HASH, 0, BENEFICIARY);
        let (second, second_receipt, second_trace) = placed(SECOND_TX_HASH, 1, TO);
        let mut value = block_value(first.clone());
        value["transactions"] = json!([first, second]);
        let raw_block = serde_json::to_vec(&value).expect("test block JSON must serialize");
        let head =
            Head::parse(&raw_block, Some(BlockHeight(10)), true).expect("test block must parse");
        let encode = |value: &Value| serde_json::to_vec(value).expect("test JSON must serialize");
        RawBlock::new(
            head.reference,
            native_network(),
            raw_block,
            vec![encode(&first_receipt), encode(&second_receipt)],
            encode(&json!([first_trace, second_trace])),
        )
    }

    #[test]
    fn interprets_successful_native_transfer_and_actual_fee() {
        let block = ethereum_block(
            transaction(Some(TO), "0x2a"),
            receipt(true, Some(TO), None, "0x3", Vec::new()),
        );
        let interpreted = inspect(&block, &[canonical_address(TO)]).expect("block must interpret");
        let draft = interpreted
            .block
            .transactions()
            .iter()
            .next()
            .expect("relevant tx must emit");
        assert_eq!(draft.movements.len(), 1);
        assert_eq!(
            draft.movements[0].id,
            MovementId(format!("{TX_HASH}:value"))
        );
        assert_eq!(draft.movements[0].amount, Decimal::from(42_u64));
        assert_eq!(
            draft.fee.as_ref().expect("fee must exist").amount,
            Decimal::from(63_000_u64)
        );
        assert_eq!(draft.movements[0].amount.scale(), 0);
        assert_eq!(
            draft.fee.as_ref().expect("fee must exist").amount.scale(),
            0
        );
        assert_eq!(draft.status, ObservationDraftStatus::Included);
    }

    /// A wallet paid through a
    /// contract appears in no transaction field and in no log, so before the
    /// trace landed this block was invisible to it. The credit must now be a
    /// movement of the transaction that caused it.
    #[test]
    fn a_watched_address_paid_by_a_contract_is_admitted_and_attributed() {
        let traces = json!([{"txHash": TX_HASH, "result": {
            "pre": {FROM: {"balance": "0xf642"}, CONTRACT: {"balance": "0x0"}},
            "post": {FROM: {"balance": "0x0"}, CONTRACT: {"balance": "0x2a"}}
        }}]);
        let block = traced_block(
            transaction(Some(TO), "0x2a"),
            receipt(true, Some(TO), None, "0x3", Vec::new()),
            traces,
        );
        let watcher = canonical_address(CONTRACT);
        let interpreted =
            inspect(&block, std::slice::from_ref(&watcher)).expect("block must interpret");
        let draft = interpreted
            .block
            .transactions()
            .iter()
            .next()
            .expect("a contract-initiated credit must admit its transaction");
        assert_eq!(
            draft.transaction_id,
            TransactionId::from_bytes(hash(TX_HASH)).canonical(&network())
        );
        let credit = draft
            .movements
            .iter()
            .find(|movement| movement.to() == Some(&watcher))
            .expect("the watched address must be credited");
        assert_eq!(credit.kind(), MovementKind::Mint);
        assert_eq!(
            credit.id,
            MovementId(format!("{TX_HASH}:residual:{CONTRACT}"))
        );
        assert_eq!(credit.amount, Decimal::from(42_u64));
        // The router the transaction names keeps nothing, so the credit it was
        // attributed is cancelled rather than left to accumulate.
        let router = canonical_address(TO);
        let debit = draft
            .movements
            .iter()
            .find(|movement| movement.from() == Some(&router))
            .expect("the named recipient must be settled");
        assert_eq!(debit.kind(), MovementKind::Burn);
        assert_eq!(debit.amount, Decimal::from(42_u64));
        // Nothing unexplained is left on the sender: its gas is the fee only.
        assert!(
            !draft
                .movements
                .iter()
                .any(|movement| movement.from() == Some(&canonical_address(FROM))
                    && movement.id.0.contains(":residual:"))
        );
        assert_eq!(
            draft.fee.as_ref().expect("fee must exist").amount,
            Decimal::from(63_000_u64)
        );
    }

    #[test]
    fn preserves_full_width_atomic_native_token_and_fee_amounts() {
        let maximum = format!("0x{}", "ff".repeat(32));
        let zero = "0x0000000000000000000000000000000000000000";
        // A sender cannot hold the whole supply and still pay for gas, so the
        // full-width value and the full-width fee are two blocks.
        let block = ethereum_block(
            transaction(Some(TO), &maximum),
            receipt(
                true,
                Some(TO),
                None,
                "0x0",
                vec![
                    log(0, FROM, TO, &maximum),
                    log(1, zero, TO, &maximum),
                    log(2, FROM, zero, &maximum),
                ],
            ),
        );
        let interpreted =
            inspect(&block, &[canonical_address(FROM)]).expect("full-width amounts must interpret");
        let draft = &interpreted.block.transactions()[0];
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
            assert_eq!(movement.amount, expected);
            assert_eq!(movement.amount.scale(), 0);
        }

        // The second fixture charges 21,000 gas at 2^240 atomic units per gas.
        let priced = ethereum_block(
            transaction(Some(TO), "0x0"),
            receipt(
                true,
                Some(TO),
                None,
                &format!("0x1{}", "0".repeat(60)),
                Vec::new(),
            ),
        );
        let interpreted =
            inspect(&priced, &[canonical_address(FROM)]).expect("a full-width fee must interpret");
        let draft = &interpreted.block.transactions()[0];
        assert!(draft.movements.is_empty());
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
            interpreted.block.transactions()[0].movements[0].to(),
            Some(&address(CONTRACT).canonical(&network()))
        );
    }

    /// A revert undoes the execution, never the fee. The only balances a
    /// failed transaction's diff can still report are the settlement: the
    /// sender's gas charge, which `fee` already states and whose residual
    /// therefore cancels, and the priority fee the block's beneficiary earned,
    /// which nothing else states. Dropping the second understates a watched
    /// fee recipient permanently, because its balance is folded from these
    /// movements and no node read corrects it.
    #[test]
    fn a_failed_receipt_moves_no_value_and_still_pays_the_beneficiary() {
        // 21 000 gas at 2 wei: 42 000 charged to the sender, of which the
        // beneficiary earns 21 000 and the base fee is burned. The 42 wei the
        // transaction carried never moved, because it reverted.
        let settlement = json!([{"txHash": TX_HASH, "result": {
            "pre": {FROM: {"balance": "0xa410"}, BENEFICIARY: {"balance": "0x0"}},
            "post": {FROM: {"balance": "0x0"}, BENEFICIARY: {"balance": "0x5208"}}
        }}]);
        let block = traced_block(
            transaction(Some(TO), "0x2a"),
            receipt(false, Some(TO), None, "0x2", Vec::new()),
            settlement,
        );
        let interpreted = inspect(
            &block,
            &[canonical_address(FROM), canonical_address(BENEFICIARY)],
        )
        .expect("failure must interpret");
        let draft = &interpreted.block.transactions()[0];
        assert!(matches!(
            draft.status,
            ObservationDraftStatus::Failed { .. }
        ));
        assert!(draft.fee.is_some());
        assert_eq!(draft.movements.len(), 1, "the settlement, and nothing else");
        assert_eq!(
            draft.movements[0].to(),
            Some(&canonical_address(BENEFICIARY))
        );
        assert_eq!(
            folded(&interpreted, canonical_address(BENEFICIARY)).amount,
            Decimal::from(21_000_u64)
        );
        // The sender pays its gas once: through `NetworkFee`, never twice.
        assert_eq!(
            folded(&interpreted, canonical_address(FROM)).completeness,
            indexing::Completeness::Overdrawn
        );
    }

    #[test]
    fn ignores_transactions_unrelated_to_the_address_filter() {
        let block = ethereum_block(
            transaction(Some(TO), "0x2a"),
            receipt(true, Some(TO), None, "0x2", Vec::new()),
        );

        let interpreted = inspect(&block, &[canonical_address(CONTRACT)])
            .expect("unrelated transaction must still interpret");

        assert!(interpreted.block.transactions().is_empty());
    }

    #[test]
    fn rejects_an_address_filter_from_another_network() {
        let block = ethereum_block(
            transaction(Some(TO), "0x2a"),
            receipt(true, Some(TO), None, "0x2", Vec::new()),
        );
        let mut foreign = canonical_address(TO);
        foreign.network.name = "other".to_owned();

        let error = inspect(&block, &[foreign]).expect_err("foreign network must be rejected");

        assert_eq!(error.kind, IndexErrorKind::NetworkMismatch);
    }

    /// One trait method, one contract: `inspect` validates the address filter
    /// for every block it parses, not only for blocks that happen to give it a
    /// reason to. This one has no transaction and no withdrawal, and used to
    /// return `Ok` on a filter belonging to another network, while bitcoin and
    /// solana refused the same call.
    #[test]
    fn an_empty_block_still_rejects_a_foreign_or_malformed_address_filter() {
        let mut foreign = canonical_address(TO);
        foreign.network.name = "other".to_owned();
        let malformed = base::Address::new(network(), "not-an-address");

        for (filter, kind) in [
            (foreign, IndexErrorKind::NetworkMismatch),
            (malformed, IndexErrorKind::InvalidRequest),
        ] {
            let error = inspect(&empty_block(), &[filter])
                .expect_err("an unusable filter is refused whatever the block holds");
            assert_eq!(error.kind, kind);
            assert!(!error.retryable);
        }

        let interpreted = inspect(&empty_block(), &[canonical_address(TO)])
            .expect("a usable filter interprets an empty block");
        assert!(interpreted.block.transactions().is_empty());
        assert!(interpreted.credits.is_empty());
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
        let movements = &interpreted.block.transactions()[0].movements;
        assert_eq!(movements.len(), 3);
        assert_eq!(movements[0].kind(), MovementKind::Transfer);
        assert_eq!(movements[1].kind(), MovementKind::Mint);
        assert_eq!(movements[1].from(), None);
        assert_eq!(movements[2].kind(), MovementKind::Burn);
        assert_eq!(movements[2].to(), None);
        assert_eq!(movements[2].id, MovementId(format!("{TX_HASH}:2")));
        for (movement, amount) in movements.iter().zip([1_u64, 2, 3]) {
            assert_eq!(movement.amount, Decimal::from(amount));
            assert_eq!(movement.amount.scale(), 0);
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
        assert!(interpreted.block.transactions()[0].movements.is_empty());
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
    fn malformed_block_and_receipt_errors_remain_terminal_before_projection() {
        let valid = ethereum_block(
            transaction(Some(TO), "0x2a"),
            receipt(true, Some(TO), None, "0x3", Vec::new()),
        );
        let block = RawBlock::new(
            valid.reference().clone(),
            valid.network().clone(),
            valid.block().to_vec(),
            vec![b"not-json".to_vec()],
            valid.traces().to_vec(),
        );
        let error =
            inspect(&block, &[]).expect_err("invalid receipt must fail even without filters");
        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
        assert_eq!(error.message, "Ethereum receipt result is not valid JSON");
        assert!(!error.retryable);

        let block = RawBlock::new(
            block.reference().clone(),
            block.network().clone(),
            b"not-json".to_vec(),
            Vec::new(),
            Vec::new(),
        );
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
    fn fee_overflow_precedes_address_filters_for_successful_and_failed_receipts() {
        let malformed = base::Address::new(network(), "invalid");
        let mut foreign = malformed.clone();
        foreign.network.name = "foreign".to_owned();
        for succeeded in [false, true] {
            let block = ethereum_block(
                transaction(Some(TO), "0x0"),
                receipt(
                    succeeded,
                    Some(TO),
                    None,
                    &format!("0x{}", "f".repeat(64)),
                    Vec::new(),
                ),
            );
            for filters in [vec![], vec![malformed.clone()], vec![foreign.clone()]] {
                let error = inspect(&block, &filters).unwrap_err();
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
