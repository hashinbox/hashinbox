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
