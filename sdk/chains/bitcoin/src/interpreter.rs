//! [`indexing::BlockInterpreter`]: parsed native facts → canonical observations
//! and spendable-output changes.

use std::collections::{BTreeMap, BTreeSet};

use base::{MovementKind, Networked};
use indexing::{
    BlockInterpreter as Interpret, IndexError, IndexErrorKind, IndexedOutput, InterpretedBlock,
    MovementId, Network, NetworkFee, ObservationDraft, ObservationDraftStatus, OutputChanges,
    OutputKey, ValueMovement,
};

use crate::native::{
    Address, AssetKind, CHAIN_NAME,
    block::{Fee, Movement, Outpoint, Output, RawBlock, Transaction},
};

/// Interprets Bitcoin blocks for one `(bitcoin, network)` network.
///
/// Besides observations it materializes the UTXOs of selected addresses:
/// outputs they receive are created, outputs they spend are removed, and
/// spends of other P2WPKH/P2TR addresses are tracked so outputs indexed
/// earlier are removed too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockInterpreter {
    network: Network,
}

impl BlockInterpreter {
    /// # Errors
    ///
    /// `NetworkMismatch` unless `network` names a Bitcoin network.
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

    /// The selected addresses: canonical text of P2WPKH or P2TR addresses of
    /// the block's network.
    fn selection(
        &self,
        block: &RawBlock,
        addresses: &[base::Address],
    ) -> Result<BTreeSet<Address>, IndexError> {
        let selected = indexing::select_addresses::<Address>(&self.network, addresses)?;
        let rejected =
            |message: &str| IndexError::new(IndexErrorKind::InvalidRequest, message, false);
        for address in &selected {
            let script = address
                .script_pubkey(block.network())
                .map_err(|_| rejected("Bitcoin indexed address is invalid or wrong-network"))?;
            if !script.is_p2wpkh() && !script.is_p2tr() {
                return Err(rejected(
                    "Bitcoin indexing supports P2WPKH and P2TR addresses only",
                ));
            }
        }
        Ok(selected)
    }

    /// `order` is the transaction's place in the block's **whole** `tx` array,
    /// which is why [`Interpret::inspect`] counts the loop rather than the
    /// transactions it keeps: the same transaction must carry the same ordinal
    /// under every address selection.
    fn observation(&self, transaction: &Transaction, order: u64) -> ObservationDraft {
        ObservationDraft {
            transaction_id: transaction.id().canonical(&self.network),
            transaction_order: order,
            status: ObservationDraftStatus::Included,
            movements: transaction
                .movements()
                .into_iter()
                .map(|movement| self.movement(movement))
                .collect(),
            fee: transaction.fee().map(|fee| self.fee(fee)),
        }
    }

    fn movement(&self, movement: Movement) -> ValueMovement {
        let (id, asset) = (MovementId(movement.id), AssetKind::Native.id());
        let amount = movement.amount.atomic();
        let owner = |address: Option<Address>| address.map(|value| value.canonical(&self.network));
        match movement.kind {
            MovementKind::Input => ValueMovement::input(id, asset, amount, owner(movement.from)),
            _ => ValueMovement::output(id, asset, amount, owner(movement.to)),
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

    /// Bitcoin's fee is the difference between the inputs a transaction spends
    /// and the outputs it creates, and this interpreter emits every one of
    /// them, so `NetworkFee` is already inside the movements: a balance fold
    /// that applied it again would charge it twice.
    fn settlement(&self) -> indexing::FeeSettlement {
        indexing::FeeSettlement::Movements
    }

    /// The evidence is parsed first, then the address filter is checked —
    /// always, whatever the block turns out to hold. One trait method answers
    /// one question one way on every chain: a block this interpreter cannot
    /// read is an `InvalidBlock` before anything is asked about the filter,
    /// and a filter of another network is a `NetworkMismatch` even for a block
    /// with nothing in it (`docs/DEFECT_PATTERNS.md` section 2.3). Bitcoin's
    /// raw evidence names its own network, so that one comparison precedes
    /// even the parse; the other two chains have nothing to compare.
    fn inspect(
        &self,
        block: &RawBlock,
        addresses: &[base::Address],
    ) -> Result<InterpretedBlock, IndexError> {
        if block.network().canonical() != self.network {
            return Err(IndexError::new(
                IndexErrorKind::NetworkMismatch,
                "Bitcoin block belongs to another network than the interpreter",
                false,
            ));
        }
        let parsed = block.parse()?;
        let selected = self.selection(block, addresses)?;
        let mut changes = Changes {
            network: &self.network,
            height: parsed.reference().height,
            selected: &selected,
            created: BTreeMap::new(),
            spent: BTreeMap::new(),
            tracked: BTreeMap::new(),
        };
        let mut transactions = Vec::new();
        // Enumerated over the complete, block-ordered list; the filter is inside
        // the loop, so `order` is the transaction's native place whatever the
        // selection keeps. The array's length is checked against the block's own
        // `nTx` before it is parsed at all.
        for (order, transaction) in (0_u64..).zip(parsed.transactions()) {
            if changes.apply(transaction)? {
                transactions.push(self.observation(transaction, order));
            }
        }
        Ok(InterpretedBlock {
            block: block.reference().clone(),
            transactions,
            outputs: changes.into_outputs(),
            credits: Vec::new(),
        })
    }
}

/// The block's net UTXO changes, keyed by outpoint.
struct Changes<'a> {
    network: &'a Network,
    height: base::BlockHeight,
    selected: &'a BTreeSet<Address>,
    created: BTreeMap<Outpoint, IndexedOutput>,
    spent: BTreeMap<Outpoint, OutputKey>,
    tracked: BTreeMap<Outpoint, OutputKey>,
}

impl Changes<'_> {
    /// Records `transaction`'s spends and receipts; true when it touches a
    /// selected address.
    fn apply(&mut self, transaction: &Transaction) -> Result<bool, IndexError> {
        let mut relevant = false;
        for input in &transaction.inputs {
            let Some((previous, address)) = Self::owned(input.previous.as_ref()) else {
                continue;
            };
            let key = OutputKey {
                address: address.canonical(self.network),
                output: input.outpoint.canonical(self.network),
            };
            if self.selected.contains(address) {
                relevant = true;
                self.spend(input.outpoint, key)?;
            } else if previous.script().is_p2wpkh() || previous.script().is_p2tr() {
                self.track(input.outpoint, key)?;
            }
        }
        for (output_index, output) in (0_u32..).zip(&transaction.outputs) {
            let Some((_, address)) = Self::owned(Some(output)) else {
                continue;
            };
            if self.selected.contains(address) {
                relevant = true;
                let outpoint = Outpoint {
                    transaction_id: *transaction.id(),
                    output_index,
                };
                self.create(outpoint, output, address, transaction.coinbase)?;
            }
        }
        Ok(relevant)
    }

    fn owned(output: Option<&Output>) -> Option<(&Output, &Address)> {
        let output = output?;
        Some((output, output.address.as_ref()?))
    }

    fn create(
        &mut self,
        outpoint: Outpoint,
        output: &Output,
        address: &Address,
        coinbase: bool,
    ) -> Result<(), IndexError> {
        let indexed = IndexedOutput {
            id: outpoint.canonical(self.network),
            address: address.canonical(self.network),
            asset: AssetKind::Native.id(),
            amount: output.value.atomic(),
            evidence: output.script_pubkey.clone(),
            created_at: self.height,
            coinbase,
        };
        if self.created.insert(outpoint, indexed).is_some() {
            return Err(IndexError::invalid_block(
                "Bitcoin block creates a duplicate indexed outpoint",
            ));
        }
        Ok(())
    }

    /// An indexed output created and spent within this block never existed
    /// outside it: its movements remain, but the UTXO state does not change.
    fn spend(&mut self, outpoint: Outpoint, key: OutputKey) -> Result<(), IndexError> {
        if self.created.remove(&outpoint).is_some() {
            return Ok(());
        }
        if self.spent.insert(outpoint, key).is_some() {
            return Err(IndexError::invalid_block(
                "Bitcoin block spends an indexed outpoint more than once",
            ));
        }
        Ok(())
    }

    fn track(&mut self, outpoint: Outpoint, key: OutputKey) -> Result<(), IndexError> {
        if self.created.remove(&outpoint).is_some() {
            return Ok(());
        }
        if self.tracked.insert(outpoint, key).is_some() {
            return Err(IndexError::invalid_block(
                "Bitcoin block contains the same tracked spend more than once",
            ));
        }
        Ok(())
    }

    fn into_outputs(self) -> OutputChanges {
        OutputChanges {
            created: self.created.into_values().collect(),
            spent: self.spent.into_values().collect(),
            tracked_spends: self.tracked.into_values().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use base::{BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, Decimal};
    use bitcoin::{OutPoint, ScriptBuf, Txid, hashes::Hash};
    use serde_json::{Value, json};

    use super::*;
    use crate::native::{
        AddressType, KeyPair,
        rpc::fixture::{hash, native, verbose},
    };

    fn network() -> Network {
        crate::Network::Regtest.canonical()
    }

    fn address(byte: u8, kind: AddressType) -> Address {
        KeyPair::import(crypto::SecretBytes::new([byte; 32]))
            .unwrap()
            .address(crate::Network::Regtest, kind)
    }

    fn script(address: &Address) -> ScriptBuf {
        address.script_pubkey(crate::Network::Regtest).unwrap()
    }

    fn compact(value: u64, address: &Address) -> Value {
        json!({"value_satoshis": value, "address": address.as_str()})
    }

    /// A block at height 10 holding `transactions`, each paired with the
    /// compact prevouts of its inputs (`None` for same-block spends).
    fn block(transactions: Vec<(bitcoin::Transaction, Vec<Option<Value>>)>) -> RawBlock {
        let values: Vec<_> = transactions
            .iter()
            .map(|(transaction, prevouts)| {
                let mut value = verbose(transaction);
                for (index, prevout) in prevouts.iter().enumerate() {
                    if let Some(prevout) = prevout {
                        value["vin"][index]["prevout"] = prevout.clone();
                    }
                }
                value
            })
            .collect();
        let value = json!({
            "hash": hash(2), "height": 10, "previousblockhash": hash(3), "time": 100,
            "nTx": values.len(), "tx": values,
        });
        let reference = BlockRef {
            position: BlockPosition(10),
            height: BlockHeight(10),
            hash: BlockHash(hash_bytes(2)),
            parent: Some(BlockParent {
                position: BlockPosition(9),
                hash: BlockHash(hash_bytes(3)),
            }),
            timestamp: Some(100),
        };
        RawBlock::new(
            reference,
            crate::Network::Regtest,
            serde_json::to_vec(&value).unwrap(),
        )
    }

    fn hash_bytes(number: u8) -> Vec<u8> {
        let mut bytes = [0_u8; 32];
        bytes[0] = number;
        bytes.to_vec()
    }

    fn inspect(block: &RawBlock, selected: &[&Address]) -> Result<InterpretedBlock, IndexError> {
        let addresses: Vec<_> = selected
            .iter()
            .map(|address| address.canonical(&network()))
            .collect();
        BlockInterpreter::new(&network())
            .unwrap()
            .inspect(block, &addresses)
    }

    fn external(byte: u8, vout: u32) -> OutPoint {
        OutPoint::new(Txid::from_byte_array([byte; 32]), vout)
    }

    #[test]
    fn construction_requires_a_bitcoin_network() {
        let foreign = Network::new(indexing::ChainName::new("ethereum"), "regtest");
        let error = BlockInterpreter::new(&foreign).unwrap_err();
        assert_eq!(error.kind, IndexErrorKind::NetworkMismatch);
    }

    #[test]
    fn blocks_of_another_network_are_rejected() {
        let interpreter = BlockInterpreter::new(&crate::Network::Mainnet.canonical()).unwrap();
        let error = interpreter.inspect(&block(Vec::new()), &[]).unwrap_err();
        assert_eq!(error.kind, IndexErrorKind::NetworkMismatch);
    }

    #[test]
    fn unrelated_transactions_are_ignored() {
        let destination = address(1, AddressType::SegwitV0);
        let unrelated = address(2, AddressType::Taproot);
        let coinbase = native(vec![OutPoint::null()], vec![(10_000, script(&destination))]);
        let interpreted = inspect(&block(vec![(coinbase, Vec::new())]), &[&unrelated]).unwrap();
        assert!(interpreted.transactions.is_empty());
        assert!(interpreted.outputs.created.is_empty());
    }

    /// A draft's `transaction_order` is its place in the **block**, not its
    /// place in what `inspect` reported.
    ///
    /// The block holds three transactions and only the last touches a selected
    /// address, so an interpreter numbering the drafts it built would answer
    /// `0`. `inspect` counts the whole `tx` array instead — whose length the
    /// parse has already checked against the block's own `nTx` — so the same
    /// transaction carries the same ordinal under every selection, which is what
    /// keeps a history cursor stable
    /// (`indexing::ObservationDraft::transaction_order`).
    #[test]
    fn a_draft_carries_its_place_in_the_block_not_in_the_selection() {
        let selected = address(1, AddressType::SegwitV0);
        let other = address(2, AddressType::Taproot);
        let coinbase = native(vec![OutPoint::null()], vec![(10_000, script(&other))]);
        let unrelated = native(vec![external(9, 0)], vec![(20_000, script(&other))]);
        let paying = native(vec![external(9, 1)], vec![(30_000, script(&selected))]);
        let raw = block(vec![
            (coinbase, Vec::new()),
            (unrelated, vec![Some(compact(21_000, &other))]),
            (paying, vec![Some(compact(31_000, &other))]),
        ]);

        let narrow = inspect(&raw, &[&selected]).unwrap();
        assert_eq!(narrow.transactions.len(), 1, "one transaction is selected");
        assert_eq!(narrow.transactions[0].transaction_order, 2);

        // The wider selection reports all three, and the last keeps the very
        // ordinal it had when it was reported alone.
        let wide = inspect(&raw, &[&selected, &other]).unwrap();
        let orders: Vec<u64> = wide
            .transactions
            .iter()
            .map(|draft| draft.transaction_order)
            .collect();
        assert_eq!(orders, [0, 1, 2]);
    }

    #[test]
    fn same_block_spends_net_utxo_state_while_emitting_movements() {
        let source = address(1, AddressType::SegwitV0);
        let destination = address(2, AddressType::Taproot);
        let funding = native(vec![OutPoint::null()], vec![(50_000, script(&source))]);
        let spending = native(
            vec![OutPoint::new(funding.compute_txid(), 0)],
            vec![(49_000, script(&destination))],
        );
        let raw = block(vec![(funding, Vec::new()), (spending.clone(), vec![None])]);
        let interpreted = inspect(&raw, &[&source, &destination]).unwrap();
        assert_eq!(interpreted.transactions.len(), 2);
        let spend = &interpreted.transactions[1];
        assert_eq!(spend.movements[0].kind(), MovementKind::Input);
        assert_eq!(spend.movements[1].kind(), MovementKind::Output);
        assert_eq!(
            spend.movements[0].id,
            MovementId(format!("{}:vin:0", spending.compute_txid()))
        );
        let fee = spend.fee.as_ref().unwrap();
        assert_eq!(fee.amount, Decimal::from(1_000_u64));
        assert_eq!(fee.amount.scale(), 0);
        assert_eq!(fee.payer, Some(source.canonical(&network())));
        assert_eq!(interpreted.outputs.created.len(), 1);
        assert!(
            interpreted.outputs.spent.is_empty() && interpreted.outputs.tracked_spends.is_empty()
        );
    }

    #[test]
    fn selected_spends_are_recorded_and_other_segwit_spends_are_tracked() {
        let selected = address(1, AddressType::SegwitV0);
        let other = address(2, AddressType::Taproot);
        let spending = native(
            vec![external(9, 1), external(9, 2)],
            vec![(9_000, ScriptBuf::from_bytes(vec![0x6a]))],
        );
        let raw = block(vec![(
            spending,
            vec![
                Some(compact(5_000, &selected)),
                Some(compact(5_000, &other)),
            ],
        )]);
        let interpreted = inspect(&raw, &[&selected]).unwrap();
        assert_eq!(interpreted.transactions.len(), 1);
        assert_eq!(interpreted.outputs.spent.len(), 1);
        assert_eq!(interpreted.outputs.tracked_spends.len(), 1);
        assert_eq!(
            interpreted.transactions[0].fee.as_ref().unwrap().payer,
            None
        );
        assert!(interpreted.outputs.created.is_empty());
    }

    #[test]
    fn invalid_filters_keep_nonretryable_request_errors() {
        let canonical = address(1, AddressType::SegwitV0);
        let legacy = Address::parse(
            "mipcBbFg9gMiCh81Kj8tqqdgoZub1ZJRfn",
            crate::Network::Regtest,
        )
        .unwrap();
        let mainnet = KeyPair::import(crypto::SecretBytes::new([1; 32]))
            .unwrap()
            .address(crate::Network::Mainnet, AddressType::SegwitV0);
        for (value, message) in [
            (
                "not-an-address".to_owned(),
                "address filter is not a canonical bitcoin address",
            ),
            (
                canonical.as_str().to_uppercase(),
                "address filter is not a canonical bitcoin address",
            ),
            (
                legacy.to_string(),
                "Bitcoin indexing supports P2WPKH and P2TR addresses only",
            ),
            (
                mainnet.to_string(),
                "address filter is not a canonical bitcoin address",
            ),
        ] {
            let filter = base::Address::new(network(), value);
            let error = BlockInterpreter::new(&network())
                .unwrap()
                .inspect(&block(Vec::new()), &[filter])
                .unwrap_err();
            assert_eq!(
                (error.kind, error.message.as_str()),
                (IndexErrorKind::InvalidRequest, message)
            );
            assert!(!error.retryable);
        }
    }

    /// One trait method, one contract: the address filter is checked for every
    /// block, not only for blocks that give the interpreter a reason to look at
    /// it. The block here has no transactions at all, and every chain answers
    /// the same way (`docs/DEFECT_PATTERNS.md` section 2.3).
    #[test]
    fn an_empty_block_still_rejects_an_address_filter_from_another_network() {
        let mine = address(1, AddressType::SegwitV0);
        let mut foreign = mine.canonical(&network());
        foreign.network.name = "mainnet".to_owned();

        let error = BlockInterpreter::new(&network())
            .unwrap()
            .inspect(&block(Vec::new()), &[foreign])
            .expect_err("a foreign filter is refused whatever the block holds");
        assert_eq!(error.kind, IndexErrorKind::NetworkMismatch);
        assert!(!error.retryable);

        let interpreted = inspect(&block(Vec::new()), &[&mine])
            .expect("a usable filter interprets an empty block");
        assert!(interpreted.transactions.is_empty());
        assert!(interpreted.outputs.created.is_empty());
    }

    #[test]
    fn invalid_evidence_is_a_terminal_invalid_block() {
        let owner = address(1, AddressType::SegwitV0);
        let spending = native(vec![external(3, 0)], vec![(2_000, script(&owner))]);
        let raw = block(vec![(spending, vec![Some(compact(1_000, &owner))])]);
        let error = inspect(&raw, &[&owner]).unwrap_err();
        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
        assert_eq!(
            error.message,
            "Bitcoin transaction outputs exceed its resolved inputs"
        );
        assert!(!error.retryable);
    }
}
