//! Spendable outputs, for the chains that have them: `OutputId` and the
//! address-qualified `OutputKey`, the `IndexedOutput` a block created, and the
//! `OutputChanges` one block applies. `evidence` is opaque chain-owned bytes
//! that indexing stores and never interprets.
//!
//! Reading them back is here too — `OutputRequest`, `OutputCursor`,
//! `OutputPage`: [`crate::OutputStore`] names them and `Service` answers it as
//! well as a store does, so they belong beside the output itself.

use base::Decimal;

use std::collections::BTreeSet;

use crate::{
    Address, AssetId, BlockHeight, BlockRef, IndexError, IndexErrorKind, Network, TransactionRef,
    history::require_limit, holder::require_address,
};

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

/// Stable position in a snapshot-consistent output listing.
///
/// `position` is opaque to callers. It may only be returned to the same
/// [`crate::OutputStore`] implementation in a later request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputCursor {
    /// The checkpoint of the first page; a later page at another checkpoint
    /// is a retryable `Conflict`.
    pub checkpoint: Option<BlockRef>,
    /// Backend-owned resume position.
    pub position: Vec<u8>,
    /// The revision of the pending overlay the page this cursor follows was
    /// folded over, when the answering [`crate::OutputStore`] folds one.
    ///
    /// The durable checkpoint above cannot carry this: a block held above the
    /// confirmed line does not move the checkpoint, so two pages can name the
    /// same one and still have been folded over different pending sets. A
    /// `crate::Service` sets it on every cursor it issues and refuses a later
    /// page whose value is not the overlay it is now holding — the spendable set
    /// is paged by coin selection, so the union of two pages folded over
    /// different pending blocks could hold an input *and* the change that exists
    /// only because that input was spent.
    ///
    /// `None` on a cursor issued by a store that folds nothing, which is every
    /// backend: the field is set by whoever did the folding, and a backend
    /// neither reads it nor returns it.
    pub revision: Option<u64>,
}

/// One page request for an address's live outputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputRequest {
    /// The network the repository is bound to.
    pub network: Network,
    /// Whose outputs to list.
    pub address: Address,
    /// Resume after this cursor, or start from the first output.
    pub after: Option<OutputCursor>,
    /// Maximum outputs on the page.
    pub limit: usize,
}

impl OutputRequest {
    /// Rejects a request no store can answer: an address of another network than
    /// the one being read, a blank address, or a page length outside
    /// `1..=1000`.
    ///
    /// The request names the network and the address both, so they can
    /// disagree, and one store serves every network: everything here is checked
    /// once, by the request, rather than by each backend that would otherwise
    /// restate it — and two backends restating a check are two chances for them
    /// to disagree about what a caller may ask for.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::NetworkMismatch`] when the address is of another
    /// network, [`IndexErrorKind::InvalidRequest`] for a blank address or a
    /// limit out of range.
    pub fn validate(&self) -> Result<(), IndexError> {
        if !self.address.belongs_to(&self.network) {
            return Err(IndexError::new(
                IndexErrorKind::NetworkMismatch,
                "an output request names an address of another network",
                false,
            ));
        }
        require_address(&self.address)?;
        require_limit(self.limit)
    }
}

/// One checkpoint-consistent page of live outputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputPage {
    /// The checkpoint the page was read at.
    pub checkpoint: Option<BlockRef>,
    /// Unspent outputs, in backend order.
    pub outputs: Vec<IndexedOutput>,
    /// Cursor for the next page, or `None` on the last one.
    pub next: Option<OutputCursor>,
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

#[cfg(test)]
mod tests {
    use base::ChainName;

    use super::*;

    fn network(name: &str) -> Network {
        Network::new(ChainName::new("test"), name)
    }

    fn request(address: &str, limit: usize) -> OutputRequest {
        OutputRequest {
            network: network("mainnet"),
            address: Address::new(network("mainnet"), address),
            after: None,
            limit,
        }
    }

    /// Everything one request can be wrong about, refused by the request.
    ///
    /// The blank address is the half a store used to answer with an empty page
    /// while the other store refused it.
    #[test]
    fn a_request_states_the_network_the_address_and_the_page() {
        request("a", 1).validate().expect("one output");
        let mut foreign = request("a", 1);
        foreign.address = Address::new(network("testnet"), "a");
        assert_eq!(
            foreign.validate().expect_err("another network").kind,
            IndexErrorKind::NetworkMismatch,
        );
        for refused in [request("a", 0), request("a", 1_001), request("   ", 10)] {
            let error = refused.validate().expect_err("an unanswerable request");
            assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
            assert!(!error.retryable);
        }
    }
}
