//! [`indexing::BlockInterpreter`]: parsed native facts → canonical observations.

use std::collections::BTreeSet;

use base::{MovementKind, Networked};
use indexing::{
    BlockInterpreter as Interpret, BlockRef, BlockWithOutputs, IndexError, MovementId, Network,
    NetworkFee, Status, ValueMovement,
};

use crate::native::{
    Address, AssetKind, CHAIN_NAME,
    block::{Fee, Movement, RawBlock, Transaction},
};

/// Interprets Solana blocks for one `(solana, network)` network: every native
/// SOL change and network fee of the selected addresses — decoded System
/// transfers, and the residual a contract call leaves on one account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockInterpreter {
    network: Network,
}

impl BlockInterpreter {
    /// # Errors
    ///
    /// `NetworkMismatch` unless `network` names a Solana network.
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

    /// Whether `transaction` concerns a selected address: a failed one only
    /// through the fee its payer paid.
    fn selects(transaction: &Transaction, selected: &BTreeSet<Address>) -> bool {
        selected.contains(transaction.payer())
            || transaction.succeeded()
                && selected.iter().any(|address| transaction.touches(address))
    }

    /// `order` is the transaction's place in the block's **whole**
    /// `transactions` array, which is why [`Interpret::inspect`] counts the loop
    /// rather than the transactions it keeps: the same transaction must carry
    /// the same ordinal under every address selection. Solana has no other
    /// meaningful order within a slot — a first signature sorts by nothing a
    /// caller cares about. `block` is the block being inspected.
    fn observation(
        &self,
        transaction: &Transaction,
        order: u64,
        block: &BlockRef,
    ) -> indexing::Transaction {
        let block = block.clone();
        let status = if transaction.succeeded() {
            Status::Included { block }
        } else {
            Status::Failed {
                block,
                reason: Some("Solana transaction execution failed".to_owned()),
            }
        };
        indexing::Transaction {
            id: transaction.id().canonical(&self.network),
            movements: transaction
                .movements()
                .iter()
                .filter_map(|movement| self.movement(movement))
                .collect(),
            fee: transaction.fee().map(|fee| self.fee(fee)),
            order,
            status,
        }
    }

    /// `None` only for a movement without the endpoints its kind requires,
    /// which the block parser never produces.
    fn movement(&self, movement: &Movement) -> Option<ValueMovement> {
        let id = MovementId(movement.id.clone());
        let asset = movement.asset.id();
        let amount = movement.amount.atomic();
        let (from, to) = (
            self.endpoint(movement.from.as_ref()),
            self.endpoint(movement.to.as_ref()),
        );
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

    fn endpoint(&self, address: Option<&Address>) -> Option<base::Address> {
        address.map(|address| address.canonical(&self.network))
    }

    fn fee(&self, fee: &Fee) -> NetworkFee {
        NetworkFee {
            asset: AssetKind::Native.id(),
            amount: fee.amount.atomic(),
            payer: self.endpoint(fee.payer.as_ref()),
        }
    }
}

impl Interpret for BlockInterpreter {
    type Block = RawBlock;

    /// The fee is charged outside the System transfer, so a balance fold
    /// debits it from the fee payer, in lamports only.
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
    ) -> Result<BlockWithOutputs, IndexError> {
        let parsed = block.parse()?;
        let selected: BTreeSet<Address> = indexing::select_addresses(&self.network, addresses)?;
        let reference = block.reference().clone();
        let mut transactions = Vec::new();
        // Enumerated over the complete, block-ordered list `getBlock` returned
        // under `transactionDetails: "full"`; the filter is inside the loop, so
        // `order` is the transaction's native place whatever the selection
        // keeps.
        for (order, transaction) in (0_u64..).zip(parsed.transactions()) {
            if transaction.succeeded() {
                transaction.reconcile(&selected)?;
            }
            if Self::selects(transaction, &selected) {
                transactions.push(self.observation(transaction, order, &reference));
            }
        }
        BlockWithOutputs::new(parsed.network(), reference, transactions)
    }
}

#[cfg(test)]
mod tests {
    use base::BlockPosition;
    use indexing::{IndexErrorKind, MovementId};
    use serde_json::{Value, json};
    use solana_address::Address as NativeAddress;
    use solana_signature::Signature;
    use solana_system_interface::{instruction::SystemInstruction, program::ID as SYSTEM_ID};

    use super::*;

    fn network() -> Network {
        Network {
            chain: crate::CHAIN_NAME,
            name: "localnet".to_owned(),
        }
    }

    fn address(byte: u8) -> String {
        NativeAddress::from([byte; 32]).to_string()
    }

    fn signature(byte: u8) -> String {
        Signature::from([byte; 64]).to_string()
    }

    fn system_data(instruction: SystemInstruction) -> String {
        bs58::encode(bincode::serialize(&instruction).expect("system instruction encodes"))
            .into_string()
    }

    fn compiled(program: u8, accounts: &[u8], instruction: SystemInstruction) -> Value {
        json!({
            "programIdIndex": program,
            "accounts": accounts,
            "data": system_data(instruction),
        })
    }

    fn transfer(program: u8, source: u8, destination: u8, lamports: u64) -> Value {
        compiled(
            program,
            &[source, destination],
            SystemInstruction::Transfer { lamports },
        )
    }

    /// An instruction of a program this crate does not decode.
    fn opaque(program: u8, accounts: &[u8]) -> Value {
        json!({"programIdIndex": program, "accounts": accounts, "data": "opaque"})
    }

    fn transaction(
        signature_byte: u8,
        keys: Vec<String>,
        instructions: Vec<Value>,
        pre: Vec<u64>,
        post: Vec<u64>,
        fee: u64,
    ) -> Value {
        json!({
            "transaction": {
                "signatures": [signature(signature_byte)],
                "message": {
                    "header": {
                        "numRequiredSignatures": 1,
                        "numReadonlySignedAccounts": 0,
                        "numReadonlyUnsignedAccounts": 1,
                    },
                    "accountKeys": keys,
                    "recentBlockhash": "11111111111111111111111111111111",
                    "instructions": instructions,
                },
            },
            "meta": {
                "err": null,
                "fee": fee,
                "preBalances": pre,
                "postBalances": post,
                "innerInstructions": [],
            },
            "version": "legacy",
        })
    }

    fn baseline(signature_byte: u8) -> Value {
        transaction(
            signature_byte,
            vec![address(1), address(2), SYSTEM_ID.to_string()],
            vec![transfer(2, 0, 1, 10)],
            vec![100, 0, 0],
            vec![83, 10, 0],
            7,
        )
    }

    fn block(transactions: Vec<Value>) -> RawBlock {
        let raw = serde_json::to_vec(&json!({
            "blockhash": "11111111111111111111111111111111",
            "previousBlockhash": "11111111111111111111111111111111",
            "parentSlot": 6,
            "transactions": transactions,
            "blockTime": 123,
            "blockHeight": 4,
        }))
        .expect("fixture block serializes");
        let genesis = "11111111111111111111111111111111";
        let network = crate::native::Network::new("localnet", genesis.parse().expect("a hash"));
        RawBlock::new(BlockPosition(7), network, raw).expect("fixture block parses")
    }

    fn selected(byte: u8) -> base::Address {
        base::Address {
            network: network(),
            value: address(byte),
        }
    }

    /// The mint every token fixture below moves.
    const MINT: u8 = 9;

    fn token_program() -> String {
        Address::from_bytes(crate::native::spl::TOKEN_PROGRAM).to_string()
    }

    fn mint_asset() -> base::AssetId {
        base::AssetId {
            chain: crate::CHAIN_NAME,
            asset: address(MINT),
        }
    }

    /// One `owner`-owned token account of [`MINT`] at account key `index`,
    /// holding `amount` units on one side of execution.
    fn token_balance(index: u8, owner: u8, amount: &str) -> Value {
        json!({
            "accountIndex": index,
            "mint": address(MINT),
            "owner": address(owner),
            "programId": token_program(),
            "uiTokenAmount": {
                "amount": amount, "decimals": 6, "uiAmount": 0.4, "uiAmountString": "0.4",
            },
        })
    }

    /// An SPL Token instruction of `tag` over `accounts`, carrying `amount`.
    fn token_instruction(tag: u8, accounts: &[u8], amount: u64) -> Value {
        let mut data = vec![tag];
        data.extend(amount.to_le_bytes());
        json!({
            "programIdIndex": 3,
            "accounts": accounts,
            "data": bs58::encode(data).into_string(),
        })
    }

    fn token_transfer(source: u8, destination: u8, amount: u64) -> Value {
        token_instruction(3, &[source, destination, 0], amount)
    }

    /// A transaction whose keys are the payer, two token accounts, the SPL
    /// Token program, and the System program, paying a 5000-lamport fee and
    /// otherwise leaving every lamport where it was.
    fn token_value(signature_byte: u8, pre: Value, post: Value, instructions: Vec<Value>) -> Value {
        let mut value = transaction(
            signature_byte,
            vec![
                address(1),
                address(5),
                address(6),
                token_program(),
                SYSTEM_ID.to_string(),
            ],
            instructions,
            vec![100_000, 2_039_280, 2_039_280, 1, 1],
            vec![95_000, 2_039_280, 2_039_280, 1, 1],
            5_000,
        );
        value["transaction"]["message"]["header"]["numReadonlyUnsignedAccounts"] = json!(2);
        value["meta"]["preTokenBalances"] = pre;
        value["meta"]["postTokenBalances"] = post;
        value
    }

    #[test]
    fn indexes_a_token_transfer_with_its_mint_amount_and_transaction() {
        let value = token_value(
            50,
            json!([token_balance(1, 1, "1000000"), token_balance(2, 2, "0")]),
            json!([token_balance(1, 1, "400000"), token_balance(2, 2, "600000")]),
            vec![token_transfer(1, 2, 600_000)],
        );
        let reported = inspect(vec![value], &[selected(2)])
            .expect("a watched owner is reached through the token balances alone");
        let draft = &reported.block.transactions()[0];
        assert_eq!(draft.id.id.as_str(), signature(50));
        assert!(matches!(draft.status, Status::Included { .. }));
        assert_eq!(draft.movements.len(), 1);
        let movement = &draft.movements[0];
        assert_eq!(movement.id, MovementId(format!("{}:ix:0", signature(50))));
        assert_eq!(movement.kind(), MovementKind::Transfer);
        assert_eq!(movement.asset, mint_asset());
        assert_eq!(movement.amount.to_string(), "600000");
        assert_eq!(movement.amount.scale(), 0);
        // The owners, not the token accounts at keys 1 and 2.
        assert_eq!(movement.from(), Some(&selected(1)));
        assert_eq!(movement.to(), Some(&selected(2)));
        let fee = draft.fee.as_ref().expect("exact fee");
        assert_eq!(fee.asset, base::AssetId::native(crate::CHAIN_NAME));
        assert_eq!(fee.amount.to_string(), "5000");
    }

    #[test]
    fn a_created_or_closed_token_account_counts_its_missing_side_as_zero() {
        // Minted into a token account this transaction created: no pre entry.
        let created = token_value(
            51,
            json!([]),
            json!([token_balance(2, 2, "600000")]),
            vec![token_instruction(7, &[2, 0], 600_000)],
        );
        let reported = inspect(vec![created], &[selected(2)])
            .expect("a missing pre entry is zero, not a rejection");
        let draft = &reported.block.transactions()[0];
        assert_eq!(draft.movements.len(), 1);
        assert_eq!(
            draft.movements[0].id,
            MovementId(format!("{}:token:2", signature(51)))
        );
        assert_eq!(draft.movements[0].kind(), MovementKind::Mint);
        assert_eq!(draft.movements[0].asset, mint_asset());
        assert_eq!(draft.movements[0].to(), Some(&selected(2)));
        assert_eq!(draft.movements[0].from(), None);
        assert_eq!(draft.movements[0].amount.to_string(), "600000");

        // Burned and closed: no post entry.
        let closed = token_value(
            52,
            json!([token_balance(1, 1, "600000")]),
            json!([]),
            vec![token_instruction(8, &[1, 0], 600_000)],
        );
        let reported = inspect(vec![closed], &[selected(1)])
            .expect("a missing post entry is zero, not a rejection");
        let draft = &reported.block.transactions()[0];
        assert_eq!(draft.movements.len(), 1);
        assert_eq!(
            draft.movements[0].id,
            MovementId(format!("{}:token:1", signature(52)))
        );
        assert_eq!(draft.movements[0].kind(), MovementKind::Burn);
        assert_eq!(draft.movements[0].from(), Some(&selected(1)));
        assert_eq!(draft.movements[0].to(), None);
        assert_eq!(draft.movements[0].amount.to_string(), "600000");
    }

    #[test]
    fn absent_token_balances_cost_attribution_and_never_halt_the_chain() {
        // One side only: the transfer names a token account the transaction
        // does not report, so the side it does report is a residual.
        let partial = token_value(
            53,
            Value::Null,
            json!([token_balance(2, 2, "600000")]),
            vec![token_transfer(1, 2, 600_000)],
        );
        let reported = inspect(vec![partial], &[selected(2)])
            .expect("a null preTokenBalances must not halt indexing");
        let draft = &reported.block.transactions()[0];
        assert_eq!(draft.movements.len(), 1);
        assert_eq!(
            draft.movements[0].id,
            MovementId(format!("{}:token:2", signature(53)))
        );
        assert_eq!(draft.movements[0].kind(), MovementKind::Mint);

        // Neither side recorded, and the fields omitted entirely: the token
        // effect is invisible, the block still indexes.
        for omit in [false, true] {
            let mut value = token_value(
                54,
                Value::Null,
                Value::Null,
                vec![token_transfer(1, 2, 600_000)],
            );
            if omit {
                let meta = value["meta"].as_object_mut().expect("fixture meta");
                meta.remove("preTokenBalances");
                meta.remove("postTokenBalances");
            }
            let interpreted = inspect(vec![value], &[selected(2)])
                .expect("a node that records no token balances must not halt indexing");
            assert!(interpreted.block.transactions().is_empty());
        }
    }

    #[test]
    fn a_native_and_a_token_movement_of_one_transaction_are_both_attributed() {
        let mut value = token_value(
            55,
            json!([token_balance(1, 1, "1000000"), token_balance(2, 2, "0")]),
            json!([token_balance(1, 1, "400000"), token_balance(2, 2, "600000")]),
            vec![transfer(4, 0, 2, 10), token_transfer(1, 2, 600_000)],
        );
        value["meta"]["postBalances"] = json!([94_990, 2_039_280, 2_039_290, 1, 1]);

        let reported =
            inspect(vec![value], &[selected(2)]).expect("SOL and a token move in one transaction");
        let draft = &reported.block.transactions()[0];
        assert_eq!(
            draft
                .movements
                .iter()
                .map(|movement| (
                    movement.id.0.clone(),
                    movement.asset.clone(),
                    movement.amount.to_string()
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    format!("{}:ix:0", signature(55)),
                    base::AssetId::native(crate::CHAIN_NAME),
                    "10".to_owned()
                ),
                (
                    format!("{}:ix:1", signature(55)),
                    mint_asset(),
                    "600000".to_owned()
                ),
            ]
        );
        assert_eq!(draft.movements[0].to(), Some(&selected(6)));
        assert_eq!(draft.movements[1].to(), Some(&selected(2)));
    }

    #[test]
    fn a_token_effect_no_instruction_names_is_a_one_sided_residual_on_each_owner() {
        // A program moves units with no SPL transfer to decode: the runtime's
        // own token balances still say exactly what each owner's position did.
        let value = token_value(
            56,
            json!([token_balance(1, 1, "1000000"), token_balance(2, 2, "0")]),
            json!([token_balance(1, 1, "400000"), token_balance(2, 2, "600000")]),
            vec![token_instruction(19, &[1, 2, 0], 600_000)],
        );
        let reported = inspect(vec![value], &[selected(2)])
            .expect("an undecoded token effect is data, not a rejection");
        let draft = &reported.block.transactions()[0];
        assert_eq!(
            draft
                .movements
                .iter()
                .map(|movement| (movement.id.0.clone(), movement.kind()))
                .collect::<Vec<_>>(),
            vec![
                (format!("{}:token:1", signature(56)), MovementKind::Burn),
                (format!("{}:token:2", signature(56)), MovementKind::Mint),
            ]
        );
        assert_eq!(draft.movements[0].from(), Some(&selected(1)));
        assert_eq!(draft.movements[1].to(), Some(&selected(2)));
        assert_eq!(draft.movements[0].amount.to_string(), "600000");
        assert_eq!(draft.movements[1].amount.to_string(), "600000");
    }

    fn inspect(
        transactions: Vec<Value>,
        selected: &[base::Address],
    ) -> Result<BlockWithOutputs, IndexError> {
        BlockInterpreter::new(&network())?.inspect(&block(transactions), selected)
    }

    #[test]
    fn validates_network_and_filter_boundaries() {
        let interpreter = BlockInterpreter::new(&network()).expect("Solana network");
        assert_eq!(interpreter.network(), &network());
        assert_eq!(
            BlockInterpreter::new(&Network {
                chain: base::ChainName::new("ethereum"),
                name: "localnet".to_owned(),
            })
            .expect_err("wrong chain")
            .kind,
            IndexErrorKind::NetworkMismatch
        );
        assert!(
            BlockInterpreter::new(&Network {
                chain: crate::CHAIN_NAME,
                name: " ".to_owned(),
            })
            .is_err()
        );

        let mut foreign = selected(2);
        foreign.network.name = "other".to_owned();
        assert_eq!(
            interpreter
                .inspect(&block(vec![baseline(9)]), &[foreign])
                .expect_err("foreign filter")
                .kind,
            IndexErrorKind::NetworkMismatch
        );
    }

    /// One trait method, one contract: the address filter is checked for every
    /// block, not only for blocks that give the interpreter a reason to look at
    /// it. The block here has no transactions at all, and every chain answers
    /// the same way (`docs/DEFECT_PATTERNS.md` section 2.3).
    #[test]
    fn an_empty_block_still_rejects_an_address_filter_from_another_network() {
        let mut foreign = selected(2);
        foreign.network.name = "other".to_owned();

        let error = inspect(Vec::new(), &[foreign])
            .expect_err("a foreign filter is refused whatever the block holds");
        assert_eq!(error.kind, IndexErrorKind::NetworkMismatch);
        assert!(!error.retryable);

        let interpreted =
            inspect(Vec::new(), &[selected(2)]).expect("a usable filter interprets an empty block");
        assert!(interpreted.block.transactions().is_empty());
    }

    /// A transaction's `order` is its place in the **block**, not its
    /// place in what `inspect` reported.
    ///
    /// The block holds three transactions and only the last touches a selected
    /// address, so an interpreter numbering the drafts it built would answer
    /// `0`. `inspect` counts the whole `transactions` array `getBlock` returned
    /// under `transactionDetails: "full"` instead, so the same transaction
    /// carries the same ordinal under every selection. Solana needs this more
    /// than the other two chains do: a first signature sorts by nothing a caller
    /// cares about, so without the ordinal a slot's history has no meaningful
    /// order at all (`indexing::Transaction::order`).
    #[test]
    fn a_draft_carries_its_place_in_the_block_not_in_the_selection() {
        let paying = |signature_byte: u8, payer: u8, recipient: u8| {
            transaction(
                signature_byte,
                vec![address(payer), address(recipient), SYSTEM_ID.to_string()],
                vec![transfer(2, 0, 1, 10)],
                vec![100, 0, 0],
                vec![83, 10, 0],
                7,
            )
        };
        let transactions = vec![paying(1, 3, 4), paying(2, 5, 6), paying(3, 1, 2)];

        let narrow = inspect(transactions.clone(), &[selected(2)]).expect("one selected slot");
        assert_eq!(
            narrow.block.transactions().len(),
            1,
            "one transaction is selected"
        );
        assert_eq!(narrow.block.transactions()[0].order, 2);
        assert_eq!(narrow.block.transactions()[0].id.id.as_str(), signature(3));

        // The wider selection reports all three, and the last keeps the very
        // ordinal it had when it was reported alone.
        let wide = inspect(transactions, &[selected(2), selected(4), selected(6)])
            .expect("every slot selected");
        let orders: Vec<u64> = wide
            .block
            .transactions()
            .iter()
            .map(|draft| draft.order)
            .collect();
        assert_eq!(orders, [0, 1, 2]);
    }

    #[test]
    fn interprets_legacy_transfer_identity_fee_and_empty_outputs() {
        let interpreted = inspect(vec![baseline(9)], &[selected(2)]).expect("legacy transfer");
        assert!(interpreted.outputs.created.is_empty());
        assert!(interpreted.outputs.spent.is_empty());
        assert!(interpreted.outputs.tracked_spends.is_empty());
        let draft = &interpreted.block.transactions()[0];
        assert_eq!(draft.id.id.as_str(), signature(9));
        assert!(matches!(draft.status, Status::Included { .. }));
        assert_eq!(draft.movements.len(), 1);
        assert_eq!(
            draft.movements[0].id,
            MovementId(format!("{}:ix:0", signature(9)))
        );
        assert_eq!(draft.movements[0].from(), Some(&selected(1)));
        assert_eq!(draft.movements[0].to(), Some(&selected(2)));
        assert_eq!(draft.movements[0].amount.to_string(), "10");
        assert_eq!(draft.movements[0].amount.scale(), 0);
        let fee = draft.fee.as_ref().expect("exact fee");
        assert_eq!(fee.amount.to_string(), "7");
        assert_eq!(fee.payer.as_ref(), Some(&selected(1)));
    }

    #[test]
    fn resolves_version_zero_writable_then_readonly_loaded_keys() {
        let mut value = transaction(
            10,
            vec![address(1)],
            vec![transfer(2, 0, 1, 10)],
            vec![100, 0, 0],
            vec![88, 10, 0],
            2,
        );
        value["version"] = json!(0);
        value["transaction"]["message"]["header"]["numReadonlyUnsignedAccounts"] = json!(0);
        value["meta"]["loadedAddresses"] = json!({
            "writable": [address(2)],
            "readonly": [SYSTEM_ID.to_string()],
        });

        let interpreted = inspect(vec![value], &[selected(2)]).expect("version-zero transfer");
        assert_eq!(
            interpreted.block.transactions()[0].movements[0].to(),
            Some(&selected(2))
        );
    }

    #[test]
    fn keeps_distinct_fee_payer_source_and_destination() {
        let mut value = transaction(
            17,
            vec![address(1), address(3), address(2), SYSTEM_ID.to_string()],
            vec![transfer(3, 1, 2, 10)],
            vec![100, 20, 0, 0],
            vec![97, 10, 10, 0],
            3,
        );
        value["transaction"]["signatures"] = json!([signature(17), signature(18)]);
        value["transaction"]["message"]["header"]["numRequiredSignatures"] = json!(2);

        let interpreted = inspect(vec![value], &[selected(2)]).expect("distinct payer and source");
        let draft = &interpreted.block.transactions()[0];
        assert_eq!(draft.id.id.as_str(), signature(17));
        assert_eq!(
            draft.fee.as_ref().and_then(|fee| fee.payer.as_ref()),
            Some(&selected(1))
        );
        assert_eq!(draft.movements[0].from(), Some(&selected(3)));
        assert_eq!(draft.movements[0].to(), Some(&selected(2)));
    }

    #[test]
    fn preserves_outer_inner_seed_repeated_self_and_zero_occurrences() {
        let owner = NativeAddress::from([44; 32]);
        let mut value = transaction(
            11,
            vec![
                address(1),
                address(2),
                address(3),
                address(4),
                address(5),
                address(6),
                SYSTEM_ID.to_string(),
            ],
            vec![transfer(6, 0, 1, 5)],
            vec![100, 0, 10, 0, 0, 0, 0],
            vec![93, 5, 3, 3, 0, 4, 0],
            2,
        );
        value["meta"]["innerInstructions"] = json!([{
            "index": 0,
            "instructions": [
                transfer(6, 2, 3, 3),
                compiled(6, &[2, 4, 5], SystemInstruction::TransferWithSeed {
                    lamports: 4,
                    from_seed: "seed".to_owned(),
                    from_owner: owner,
                }),
                transfer(6, 1, 1, 2),
                transfer(6, 0, 1, 0),
            ],
        }]);

        let interpreted = inspect(vec![value], &[selected(2)]).expect("complete inner transfers");
        let movements = &interpreted.block.transactions()[0].movements;
        assert_eq!(movements.len(), 4);
        assert_eq!(movements[1].from(), Some(&selected(3)));
        assert_eq!(movements[1].to(), Some(&selected(4)));
        assert_eq!(movements[2].to(), Some(&selected(6)));
        assert_ne!(movements[2].to(), Some(&selected(5)));
        assert_eq!(movements[3].from(), movements[3].to());
        assert_eq!(
            movements
                .iter()
                .map(|movement| movement.id.0.clone())
                .collect::<Vec<_>>(),
            vec![
                format!("{}:ix:0", signature(11)),
                format!("{}:ix:0:inner:0", signature(11)),
                format!("{}:ix:0:inner:1", signature(11)),
                format!("{}:ix:0:inner:2", signature(11)),
            ]
        );
    }

    #[test]
    fn failed_transaction_is_fee_only_and_visible_only_to_payer() {
        let mut value = baseline(12);
        value["meta"]["err"] = json!({"InstructionError": [0, "Custom"]});
        value["meta"]["fee"] = json!(5);
        value["meta"]["postBalances"] = json!([95, 0, 0]);
        value["meta"]["innerInstructions"] = Value::Null;

        let payer = inspect(vec![value.clone()], &[selected(1)]).expect("failed payer history");
        let draft = &payer.block.transactions()[0];
        assert!(draft.movements.is_empty());
        assert!(matches!(draft.status, Status::Failed { .. }));
        assert_eq!(
            draft.fee.as_ref().expect("failed fee").amount.to_string(),
            "5"
        );
        assert_eq!(
            draft.fee.as_ref().and_then(|fee| fee.payer.as_ref()),
            Some(&selected(1))
        );

        let endpoint = inspect(vec![value], &[selected(2)]).expect("attempted endpoint ignored");
        assert!(endpoint.block.transactions().is_empty());
    }

    #[test]
    fn retains_successful_fee_only_transaction_for_selected_payer() {
        let value = transaction(
            13,
            vec![address(1), SYSTEM_ID.to_string()],
            Vec::new(),
            vec![10, 0],
            vec![7, 0],
            3,
        );
        let interpreted = inspect(vec![value], &[selected(1)]).expect("fee-only success");
        assert_eq!(interpreted.block.transactions().len(), 1);
        assert!(interpreted.block.transactions()[0].movements.is_empty());
        assert_eq!(
            interpreted.block.transactions()[0]
                .fee
                .as_ref()
                .and_then(|fee| fee.payer.as_ref()),
            Some(&selected(1))
        );
    }

    #[test]
    fn ignores_readonly_mentions_and_transfer_lookalikes_from_other_programs() {
        let fake_program = address(90);
        let mut value = transaction(
            14,
            vec![address(1), address(2), address(8), fake_program],
            vec![transfer(3, 0, 1, 10)],
            vec![100, 0, 7, 0],
            vec![99, 10, 7, 0],
            1,
        );
        value["transaction"]["message"]["header"]["numReadonlyUnsignedAccounts"] = json!(2);
        value["meta"]["innerInstructions"] = Value::Null;

        let interpreted = inspect(vec![value], &[selected(8)]).expect("readonly mention ignored");
        assert!(interpreted.block.transactions().is_empty());
    }

    #[test]
    fn inner_instructions_the_node_did_not_report_cost_attribution_not_indexing() {
        // The node omits `innerInstructions` entirely while an inner transfer
        // moved 5 lamports from the payer to a third account. Nothing names
        // both of its endpoints, so each side becomes its own residual and the
        // block still indexes.
        let mut incomplete = transaction(
            16,
            vec![address(1), address(2), address(3), SYSTEM_ID.to_string()],
            vec![transfer(3, 0, 1, 10)],
            vec![100, 0, 0, 0],
            vec![78, 10, 5, 0],
            7,
        );
        incomplete["meta"]["innerInstructions"] = Value::Null;

        let reported = inspect(vec![incomplete], &[selected(3)])
            .expect("a node without inner instructions must not halt the chain");
        let draft = &reported.block.transactions()[0];
        let ids = draft
            .movements
            .iter()
            .map(|movement| movement.id.0.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                format!("{}:ix:0", signature(16)),
                format!("{}:residual:0", signature(16)),
                format!("{}:residual:2", signature(16)),
            ]
        );
        // The decoded outer transfer keeps both endpoints; the undecodable
        // inner one is a one-sided pair of exact amounts.
        assert_eq!(draft.movements[0].kind(), MovementKind::Transfer);
        assert_eq!(draft.movements[1].kind(), MovementKind::Burn);
        assert_eq!(draft.movements[1].from(), Some(&selected(1)));
        assert_eq!(draft.movements[1].amount.to_string(), "5");
        assert_eq!(draft.movements[2].kind(), MovementKind::Mint);
        assert_eq!(draft.movements[2].to(), Some(&selected(3)));
        assert_eq!(draft.movements[2].amount.to_string(), "5");
    }

    #[test]
    fn attributes_a_contract_lamport_effect_as_a_residual_debit_and_credit() {
        // A program moves 10 lamports from the payer to a watched account with
        // no System transfer to decode. The runtime's own accounting says what
        // moved, so it is recorded rather than halting the network's indexing.
        let value = transaction(
            15,
            vec![address(1), address(2), address(90)],
            vec![opaque(2, &[0, 1])],
            vec![100, 0, 0],
            vec![87, 10, 0],
            3,
        );

        let reported =
            inspect(vec![value], &[selected(2)]).expect("a residual is data, not a rejection");
        let draft = &reported.block.transactions()[0];
        assert_eq!(draft.id.id.as_str(), signature(15));
        assert!(matches!(draft.status, Status::Included { .. }));
        assert_eq!(draft.movements.len(), 2);
        // The payer moved 10 lamports beyond the 3 it paid in fee, which
        // `NetworkFee` still carries on its own.
        assert_eq!(draft.movements[0].kind(), MovementKind::Burn);
        assert_eq!(
            draft.movements[0].id,
            MovementId(format!("{}:residual:0", signature(15)))
        );
        assert_eq!(draft.movements[0].from(), Some(&selected(1)));
        assert_eq!(draft.movements[0].to(), None);
        assert_eq!(draft.movements[0].amount.to_string(), "10");
        assert_eq!(draft.movements[1].kind(), MovementKind::Mint);
        assert_eq!(
            draft.movements[1].id,
            MovementId(format!("{}:residual:1", signature(15)))
        );
        assert_eq!(draft.movements[1].to(), Some(&selected(2)));
        assert_eq!(draft.movements[1].from(), None);
        assert_eq!(draft.movements[1].amount.to_string(), "10");
        let fee = draft.fee.as_ref().expect("exact fee");
        assert_eq!(fee.amount.to_string(), "3");
        assert_eq!(fee.payer.as_ref(), Some(&selected(1)));
    }

    /// The settled balance of `of` in SOL, folded from `interpreted` exactly
    /// as a store folds the rows it wrote from it, under this chain's own
    /// fee rule.
    fn folded(interpreted: &BlockWithOutputs, of: base::Address) -> num_bigint::BigInt {
        use indexing::Net as _;
        let subject = indexing::Subject {
            address: of,
            asset: AssetKind::Native.id(),
        };
        let settlement = BlockInterpreter::new(&network())
            .expect("the fixture network is Solana")
            .settlement();
        interpreted
            .block
            .transactions()
            .net(&subject, settlement)
            .expect("interpreted amounts are storable")
    }

    #[test]
    fn a_contract_lamport_effect_reaches_the_balance_instead_of_being_absorbed() {
        // 10 lamports reach the watched account through a program call that
        // names no System transfer, and 5 more through one that does. The
        // balance is a fold over what was indexed, so a lamport delta left
        // unattributed would simply not be there: the fold would read 5.
        let mut value = transaction(
            31,
            vec![
                address(1),
                address(2),
                address(3),
                SYSTEM_ID.to_string(),
                address(90),
            ],
            vec![transfer(3, 0, 1, 5), opaque(4, &[2, 1])],
            vec![100, 0, 20, 0, 0],
            vec![88, 15, 10, 0, 0],
            7,
        );
        value["transaction"]["message"]["header"]["numReadonlyUnsignedAccounts"] = json!(2);
        let interpreted = inspect(vec![value], &[selected(2)]).expect("a residual is data");

        let credited = folded(&interpreted, selected(2));
        assert_eq!(credited, num_bigint::BigInt::from(15_u64));
        // 15 is exactly the account's own post − pre, which is the runtime's
        // answer and the one number the residual exists to preserve.

        // The third account funded the residual and is debited for it; a fold
        // that starts after it was funded goes negative, which is reported
        // rather than clamped silently.
        let funder = folded(&interpreted, selected(3));
        assert!(funder < num_bigint::BigInt::ZERO, "overdrawn");
    }

    #[test]
    fn keeps_a_decoded_transfer_beside_a_residual_on_the_same_account() {
        // The watched account receives 10 lamports by System transfer and 5
        // more from a program call, which a third account funded.
        let mut value = transaction(
            19,
            vec![
                address(1),
                address(2),
                address(3),
                SYSTEM_ID.to_string(),
                address(90),
            ],
            vec![transfer(3, 0, 1, 10), opaque(4, &[2, 1])],
            vec![100, 0, 20, 0, 0],
            vec![83, 15, 15, 0, 0],
            7,
        );
        value["transaction"]["message"]["header"]["numReadonlyUnsignedAccounts"] = json!(2);

        let reported = inspect(vec![value], &[selected(2)]).expect("transfer and residual coexist");
        let draft = &reported.block.transactions()[0];
        assert_eq!(
            draft
                .movements
                .iter()
                .map(|movement| (movement.id.0.clone(), movement.kind()))
                .collect::<Vec<_>>(),
            vec![
                (format!("{}:ix:0", signature(19)), MovementKind::Transfer),
                (format!("{}:residual:1", signature(19)), MovementKind::Mint),
                (format!("{}:residual:2", signature(19)), MovementKind::Burn),
            ]
        );
        assert_eq!(draft.movements[0].from(), Some(&selected(1)));
        assert_eq!(draft.movements[0].to(), Some(&selected(2)));
        assert_eq!(draft.movements[0].amount.to_string(), "10");
        assert_eq!(draft.movements[1].to(), Some(&selected(2)));
        assert_eq!(draft.movements[1].amount.to_string(), "5");
        assert_eq!(draft.movements[2].from(), Some(&selected(3)));
        assert_eq!(draft.movements[2].amount.to_string(), "5");
        // The payer's own delta is the transfer and the fee, so it has none.
        assert!(
            !draft
                .movements
                .iter()
                .any(|movement| movement.id.0.ends_with(":residual:0"))
        );
    }

    #[test]
    fn malformed_transaction_poisoning_is_all_or_nothing() {
        let mut unsupported = baseline(21);
        unsupported["version"] = json!(1);
        let mut no_meta = baseline(22);
        no_meta["meta"] = Value::Null;
        let mut bad_signature_count = baseline(23);
        bad_signature_count["transaction"]["signatures"] = json!([]);
        let mut malformed_signature = baseline(28);
        malformed_signature["transaction"]["signatures"] = json!(["not-a-signature"]);
        let mut malformed_address = baseline(29);
        malformed_address["transaction"]["message"]["accountKeys"][1] = json!("not-an-address");
        let mut bad_balances = baseline(24);
        bad_balances["meta"]["postBalances"] = json!([83, 10]);
        let mut duplicate_inner = baseline(25);
        duplicate_inner["meta"]["innerInstructions"] = json!([
            {"index": 0, "instructions": []},
            {"index": 0, "instructions": []},
        ]);
        let mut invalid_index = baseline(26);
        invalid_index["transaction"]["message"]["instructions"][0]["accounts"] = json!([0, 99]);
        let mut missing_loaded = baseline(27);
        missing_loaded["version"] = json!(0);

        for malformed in [
            unsupported,
            no_meta,
            bad_signature_count,
            malformed_signature,
            malformed_address,
            bad_balances,
            duplicate_inner,
            invalid_index,
            missing_loaded,
        ] {
            let error = inspect(vec![baseline(20), malformed], &[selected(2)])
                .expect_err("one malformed transaction must poison the block");
            assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
        }
    }

    #[test]
    fn rejects_duplicate_first_signature_identity() {
        let error = inspect(vec![baseline(30), baseline(30)], &[selected(2)])
            .expect_err("duplicate first signature");
        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
        assert!(error.message.contains("duplicate first signatures"));
    }

    #[test]
    fn shared_interpretation_errors_preserve_exact_context_and_nonretryability() {
        for (path, invalid, message) in [
            (
                "/meta",
                Value::Null,
                "Solana transaction metadata is missing",
            ),
            (
                "/meta/postBalances",
                json!([83, 10]),
                "Solana transaction balances do not match resolved account keys",
            ),
            (
                "/transaction/message/instructions/0/data",
                json!("0"),
                "Solana System instruction data is not canonical Base58",
            ),
        ] {
            let mut transaction = baseline(41);
            *transaction.pointer_mut(path).unwrap() = invalid;
            let error = inspect(vec![baseline(40), transaction], &[selected(2)]).unwrap_err();
            assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
            assert_eq!(error.message, message);
            assert!(!error.retryable);
        }
    }

    #[test]
    fn malformed_static_and_loaded_addresses_keep_context_before_balance_validation() {
        for target in ["static", "writable", "readonly"] {
            let mut value = baseline(31);
            match target {
                "static" => value["transaction"]["message"]["accountKeys"][1] = json!("invalid"),
                "writable" => {
                    value["version"] = json!(0);
                    value["meta"]["loadedAddresses"] =
                        json!({"writable":["invalid"],"readonly":[]});
                }
                "readonly" => {
                    value["version"] = json!(0);
                    value["meta"]["loadedAddresses"] =
                        json!({"writable":[],"readonly":["invalid"]});
                }
                _ => unreachable!(),
            }
            value["meta"]["preBalances"] = json!([]);
            let error = inspect(vec![value], &[selected(2)]).unwrap_err();
            assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
            assert!(!error.retryable);
            assert_eq!(
                error.message,
                "Solana transaction contains a malformed canonical address"
            );
        }
    }

    #[test]
    fn instruction_bounds_and_inner_group_validation_keep_error_precedence() {
        let index_error = "Solana compiled instruction contains an invalid account index";
        let group_error =
            "Solana transaction contains an invalid or duplicate inner-instruction group";
        for (program, accounts) in [(3, vec![0]), (2, vec![3])] {
            let invalid = json!({"programIdIndex":program,"accounts":accounts,"data":"opaque"});
            let mut outer = baseline(35);
            outer["transaction"]["message"]["instructions"] = json!([invalid.clone()]);
            let mut inner = baseline(35);
            inner["meta"]["innerInstructions"] =
                json!([{"index":0,"instructions":[invalid.clone()]}]);
            let mut invalid_group = baseline(35);
            invalid_group["meta"]["innerInstructions"] =
                json!([{"index":1,"instructions":[invalid.clone()]}]);
            let mut duplicate_group = baseline(35);
            duplicate_group["meta"]["innerInstructions"] = json!([
                {"index":0,"instructions":[]}, {"index":0,"instructions":[invalid]},
            ]);
            for (value, expected) in [
                (outer, index_error),
                (inner, index_error),
                (invalid_group, group_error),
                (duplicate_group, index_error),
            ] {
                let error = inspect(vec![value], &[selected(2)]).unwrap_err();
                assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
                assert!(!error.retryable);
                assert_eq!(error.message, expected);
            }
        }
    }

    #[test]
    fn instruction_conversion_preserves_account_order_duplicates_and_opaque_data() {
        let mut value = baseline(36);
        // Failed, so the opaque System data is kept but never decoded.
        value["meta"]["err"] = json!({"InstructionError": [0, "Custom"]});
        value["transaction"]["message"]["instructions"] = json!([
            {"programIdIndex":2,"accounts":[2,0,2,1],"data":"opaque_1"},
            {"programIdIndex":0,"accounts":[],"data":""},
        ]);
        value["meta"]["innerInstructions"] = json!([
            {"index":1,"instructions":[{"programIdIndex":1,"accounts":[1,0],"data":"opaque_2"}]},
        ]);
        let block = block(vec![value]).parse().unwrap();
        let transaction = block.transactions().iter().next().unwrap();
        let instructions = &transaction.instructions;
        assert_eq!(instructions.len(), 2);
        assert_eq!(instructions[0].program, 2);
        assert_eq!(instructions[0].accounts, [2, 0, 2, 1]);
        assert_eq!(instructions[0].data, "opaque_1");
        assert!(instructions[0].inner.is_empty());
        assert_eq!(instructions[1].program, 0);
        assert!(instructions[1].accounts.is_empty());
        assert!(instructions[1].data.is_empty());
        let inner = &instructions[1].inner[0];
        assert_eq!(inner.program, 1);
        assert_eq!(inner.accounts, [1, 0]);
        assert_eq!(inner.data, "opaque_2");
    }

    #[test]
    fn transaction_validation_preserves_header_signature_key_and_metadata_precedence() {
        let mut value = baseline(42);
        value["version"] = json!(1);
        value["transaction"]["message"]["header"]["numRequiredSignatures"] = json!(0);
        value["transaction"]["signatures"] = json!(["invalid"]);
        value["transaction"]["message"]["accountKeys"][0] = json!("invalid");
        value["meta"] = Value::Null;
        for (path, replacement, expected) in [
            (
                "/version",
                json!(1),
                "Solana block contains an unsupported transaction version",
            ),
            (
                "/version",
                json!("legacy"),
                "Solana transaction has incoherent signer or message-header cardinality",
            ),
            (
                "/transaction/message/header/numRequiredSignatures",
                json!(1),
                "Solana transaction contains a malformed signature",
            ),
            (
                "/transaction/signatures",
                json!([signature(42)]),
                "Solana transaction contains a malformed canonical address",
            ),
            (
                "/transaction/message/accountKeys/0",
                json!(address(1)),
                "Solana transaction metadata is missing",
            ),
        ] {
            *value.pointer_mut(path).unwrap() = replacement;
            let error = inspect(vec![value.clone()], &[selected(2)]).unwrap_err();
            assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
            assert_eq!(error.message, expected);
            assert!(!error.retryable);
        }
    }

    #[test]
    fn validates_every_signature_and_resolved_key_invariant_before_balances() {
        let mut signatures = baseline(43);
        signatures["transaction"]["message"]["header"]["numRequiredSignatures"] = json!(2);
        signatures["transaction"]["signatures"] = json!([signature(43), "invalid"]);
        let mut duplicate = baseline(43);
        duplicate["version"] = json!(0);
        duplicate["meta"]["loadedAddresses"] = json!({"writable":[address(1)],"readonly":[]});
        let mut oversized = duplicate.clone();
        oversized["meta"]["loadedAddresses"]["writable"] = json!(vec![address(1); 254]);
        for (mut value, expected) in [
            (
                signatures,
                "Solana transaction contains a malformed signature",
            ),
            (
                duplicate,
                "Solana transaction resolves duplicate account keys",
            ),
            (
                oversized,
                "Solana transaction resolves more than 256 account keys",
            ),
        ] {
            value["meta"]["preBalances"] = json!([]);
            let error = inspect(vec![value], &[selected(2)]).unwrap_err();
            assert_eq!(error.message, expected);
            assert!(!error.retryable);
        }
    }
}
