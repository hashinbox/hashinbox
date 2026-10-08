//! [`indexing::BlockInterpreter`]: parsed native facts → canonical observations
//! and spendable-output changes.

use std::collections::{BTreeMap, BTreeSet};

use base::{MovementKind, Networked};
use indexing::{
    BlockInterpreter as Interpret, BlockRef, BlockWithOutputs, IndexError, IndexErrorKind,
    IndexedOutput, MovementId, Network, NetworkFee, OutputChanges, OutputKey, Status,
    ValueMovement,
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
    /// under every address selection. `block` is the block being inspected,
    /// which included it.
    fn observation(
        &self,
        transaction: &Transaction,
        order: u64,
        block: &BlockRef,
    ) -> indexing::Transaction {
        indexing::Transaction {
            id: transaction.id().canonical(&self.network),
            movements: transaction
                .movements()
                .into_iter()
                .map(|movement| self.movement(movement))
                .collect(),
            fee: transaction.fee().map(|fee| self.fee(fee)),
            order,
            status: Status::Included {
                block: block.clone(),
            },
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
        indexing::FeeSettlement::Embedded
    }

    /// The evidence is parsed first, then the address filter is checked —
    /// always, whatever the block turns out to hold. One trait method answers
    /// one question one way on every chain: a block this interpreter cannot
    /// read is an `InvalidBlock` before anything is asked about the filter,
    /// and a filter of another network is a `NetworkMismatch` even for a block
    /// with nothing in it (`docs/DEFECT_PATTERNS.md` section 2.3). Bitcoin's
    /// parse reads addresses in the encoding of the network its evidence
    /// names, so that one comparison precedes even the parse; on the other two
    /// chains the network changes nothing about the parse, and the network the
    /// block states is checked when its facts are validated.
    fn inspect(
        &self,
        block: &RawBlock,
        addresses: &[base::Address],
    ) -> Result<BlockWithOutputs, IndexError> {
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
        let reference = block.reference().clone();
        let mut transactions = Vec::new();
        // Enumerated over the complete, block-ordered list; the filter is inside
        // the loop, so `order` is the transaction's native place whatever the
        // selection keeps. The array's length is checked against the block's own
        // `nTx` before it is parsed at all.
        for (order, transaction) in (0_u64..).zip(parsed.transactions()) {
            if changes.apply(transaction)? {
                transactions.push(self.observation(transaction, order, &reference));
            }
        }
        Ok(BlockWithOutputs {
            outputs: changes.into_outputs(),
            ..BlockWithOutputs::new(parsed.network(), reference, transactions)?
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
