//! Reading spendable outputs back: the [`OutputStore`] port and the
//! `OutputRequest`, `OutputCursor` and `OutputPage` it names.
//!
//! The outputs themselves — [`crate::IndexedOutput`] and the
//! [`crate::OutputChanges`] a block applies — are committed facts and stay in
//! the engine. Only the page a reader asks for is here.

use crate::storage::subject::require_address;
use crate::{
    Address, BlockRef, BoxFuture, Checkpointed, IndexError, IndexErrorKind, IndexedOutput, Network,
};

/// The longest page any output request asks for, and therefore the longest
/// any store returns.
const MAX_PAGE: usize = 1_000;

/// One page read with one row to spare, which is how a store knows whether
/// another page follows without reading twice.
#[cfg(any(feature = "sqlite", feature = "memory"))]
pub(crate) struct Probe {
    limit: usize,
}

#[cfg(any(feature = "sqlite", feature = "memory"))]
impl Probe {
    /// The probe for a page of `limit` rows.
    pub(crate) const fn new(limit: usize) -> Self {
        Self { limit }
    }

    /// How many rows to read: the page, and the one that proves another follows.
    pub(crate) const fn rows(&self) -> usize {
        self.limit.saturating_add(1)
    }

    /// The page out of the rows a store read, and where the next one resumes:
    /// after the last row returned, and only when the spare row came back.
    pub(crate) fn page<T, C>(
        &self,
        mut rows: Vec<T>,
        cursor: impl FnOnce(&T) -> C,
    ) -> (Vec<T>, Option<C>) {
        let more = rows.len() > self.limit;
        rows.truncate(self.limit);
        let next = rows.last().filter(|_| more).map(cursor);
        (rows, next)
    }
}

/// Current live outputs indexed for canonical addresses.
///
/// Its SDK consumer is `chain_bitcoin::sender::Spendable`, which folds the
/// pending blocks over these pages.
pub trait OutputStore: Send + Sync {
    /// Lists one checkpoint-consistent page of spendable outputs.
    ///
    /// # Errors
    ///
    /// A retryable [`IndexErrorKind::Conflict`] while any observation of the
    /// address is still being inspected below the checkpoint the page would be
    /// read at, or the address is observed and its network has no checkpoint,
    /// because a row cannot be proven unspent over blocks nothing has read
    /// ([`crate::Inspection::spendable`]). The same kind when the checkpoint
    /// moved under a cursor.
    fn list<'a>(&'a self, request: OutputRequest) -> BoxFuture<'a, Result<OutputPage, IndexError>>;
}

/// The forwarding impl, so a store that cannot be cloned is used as
/// `Arc<MySink>`.
impl<T: OutputStore + ?Sized> OutputStore for std::sync::Arc<T> {
    fn list<'a>(&'a self, request: OutputRequest) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
        (**self).list(request)
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
        if (1..=MAX_PAGE).contains(&self.limit) {
            return Ok(());
        }
        Err(IndexError::new(
            IndexErrorKind::InvalidRequest,
            format!("query limit must be between 1 and {MAX_PAGE}"),
            false,
        ))
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
mod outputs_tests {
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
