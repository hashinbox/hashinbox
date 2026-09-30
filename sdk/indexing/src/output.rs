//! Spendable outputs, for the chains that have them: `OutputId` and the
//! address-qualified `OutputKey`, the `IndexedOutput` a block created, and the
//! `OutputChanges` one block applies. `evidence` is opaque chain-owned bytes
//! that indexing stores and never interprets.
//!
//! These are committed facts. Reading them back — `OutputRequest`,
//! `OutputCursor`, `OutputPage` — is a repository's query and lives with the
//! other queries, under `storage/`.

use base::Decimal;

use std::collections::BTreeSet;

use crate::{Address, AssetId, BlockHeight, IndexError, IndexErrorKind, Network, TransactionRef};

/// Chain-neutral identity of one transaction output.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutputId {
    /// The creating transaction.
    pub transaction: TransactionRef,
    /// The output's index within that transaction.
    pub index: u32,
}

/// Address-qualified output identity used by materialized spendable-output indexes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutputKey {
    /// The output's owner.
    pub address: Address,
    /// The output itself.
    pub output: OutputId,
}

/// A spendable output discovered in a canonical block.
///
/// `evidence` is opaque chain-owned data required by a transaction builder,
/// such as a locking script. Indexing stores it without interpreting it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedOutput {
    /// Transaction and index that created it.
    pub id: OutputId,
    /// The owner; always a selected address.
    pub address: Address,
    /// The asset it holds; must be of the network's chain.
    pub asset: AssetId,
    /// Exact amount in the asset's smallest unit.
    pub amount: Decimal,
    /// Opaque chain-owned spending data.
    pub evidence: Vec<u8>,
    /// Height of the creating block; equals the block being added.
    pub created_at: BlockHeight,
    /// Whether it is a coinbase output (subject to chain maturity rules).
    pub coinbase: bool,
}

impl IndexedOutput {
    /// The address-qualified identity used as the output index key.
    #[must_use]
    pub fn key(&self) -> OutputKey {
        OutputKey {
            address: self.address.clone(),
            output: self.id.clone(),
        }
    }
}

/// Canonical output-index changes produced while interpreting one block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputChanges {
    /// Outputs created for selected addresses.
    pub created: Vec<IndexedOutput>,
    /// Outputs of selected addresses spent in this block.
    pub spent: Vec<OutputKey>,
    /// Spends outside the current address filter are applied only if the output
    /// is already materialized. This preserves previously indexed outputs.
    pub tracked_spends: Vec<OutputKey>,
}

impl OutputChanges {
    pub(crate) fn validate(
        &self,
        network: &Network,
        block_height: BlockHeight,
    ) -> Result<(), IndexError> {
        let mut created_ids = BTreeSet::new();
        let mut created = BTreeSet::new();
        for output in &self.created {
            if !output.address.belongs_to(network)
                || !output.id.transaction.belongs_to(network)
                || output.asset.chain != network.chain
                || output.amount.validate_amount().is_err()
                || output.created_at != block_height
                || !created_ids.insert(output.id.clone())
                || !created.insert(output.key())
            {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "block contains an invalid output",
                    false,
                ));
            }
        }
        let mut spent = BTreeSet::new();
        for key in self.spent.iter().chain(&self.tracked_spends) {
            if !key.address.belongs_to(network)
                || !key.output.transaction.belongs_to(network)
                || created.contains(key)
                || !spent.insert(key)
            {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "block contains overlapping or duplicate output changes",
                    false,
                ));
            }
        }
        Ok(())
    }
}
