//! Reading spendable outputs back: `OutputRequest`, `OutputCursor` and
//! `OutputPage`, which [`crate::OutputStore`] names.
//!
//! The outputs themselves — [`crate::IndexedOutput`] and the
//! [`crate::OutputChanges`] a block applies — are committed facts and stay in
//! the engine. Only the page a reader asks for is here.

use super::history::require_limit;
use crate::{
    Address, BlockRef, Checkpointed, IndexError, IndexErrorKind, IndexedOutput, Network,
    holder::require_address,
};

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
    /// The revision of the pending blocks the page this cursor follows was
    /// folded over, when whatever answered it folds them.
    ///
    /// The durable checkpoint above cannot carry this: a block held above the
    /// confirmed line does not move the checkpoint, so two pages can name the
    /// same one and still have been folded over different pending sets. A
    /// consumer that joins a store's page to a [`crate::PendingSample`] sets it
    /// on every cursor it issues, from [`crate::PendingSample::revision`], and
    /// refuses a later page whose value is not the sample it now holds — the
    /// spendable set is paged by coin selection, so the union of two pages
    /// folded over different pending blocks could hold an input *and* the
    /// change that exists only because that input was spent.
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

/// The page and its checkpoint are one snapshot, so the page is what a
/// pending sample is narrowed by.
impl Checkpointed for OutputPage {
    fn checkpoint(&self) -> Option<&BlockRef> {
        self.checkpoint.as_ref()
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
