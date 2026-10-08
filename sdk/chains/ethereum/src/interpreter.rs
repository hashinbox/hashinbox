//! [`indexing::BlockInterpreter`]: parsed native facts → canonical observations.

use std::collections::BTreeSet;

use base::{MovementKind, Networked};
use indexing::{
    BlockCredit, BlockInterpreter as Interpret, BlockRef, BlockWithOutputs, IndexError, MovementId,
    Network, NetworkFee, Status, ValueMovement,
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

    /// One transaction of `block` as the index keeps it.
    fn observation(
        &self,
        transaction: &Transaction,
        movements: Vec<Movement>,
        block: &BlockRef,
    ) -> indexing::Transaction {
        let block = block.clone();
        let status = if transaction.succeeded() {
            Status::Included { block }
        } else {
            Status::Failed {
                block,
                reason: Some("ethereum receipt status is zero".to_owned()),
            }
        };
        indexing::Transaction {
            id: transaction.id().canonical(&self.network),
            movements: movements
                .into_iter()
                .filter_map(|movement| self.movement(movement))
                .collect(),
            fee: transaction.fee().map(|fee| self.fee(fee)),
            // The node's own `transactionIndex`, checked against the block's
            // transaction list and against the receipt when the block was
            // parsed (`native/block/head.rs`, `native/block/receipt.rs`). Never
            // this loop's counter: `inspect` reports the selected transactions
            // only, so counting them would number the selection.
            order: transaction.listed.index,
            status,
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
    ) -> Result<BlockWithOutputs, IndexError> {
        let parsed = block.parse()?;
        let selected: BTreeSet<Address> = indexing::select_addresses(&self.network, addresses)?;
        let reference = block.reference().clone();
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
                transactions.push(self.observation(transaction, movements, &reference));
            }
        }
        let credits = block
            .withdrawals()?
            .iter()
            .filter(|withdrawal| selected.contains(&withdrawal.address))
            .map(|withdrawal| self.credit(withdrawal))
            .collect();
        Ok(BlockWithOutputs {
            credits,
            ..BlockWithOutputs::new(parsed.network(), reference, transactions)?
        })
    }
}
