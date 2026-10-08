//! Blocks as retained evidence ([`RawBlock`]) and as parsed facts ([`Block`]).
//!
//! `RawBlock` → [`RawBlock::parse`] → [`Block`] → [`Transaction`] →
//! [`Transaction::movements`] → [`Movement`]. Parsing happens only here.

mod field;
mod head;
mod log;
mod receipt;
mod trace;
mod transaction;
mod withdrawal;

use base::BlockRef;

pub(crate) use head::Head;
pub use head::ListedTransaction;
pub use log::Log;
pub use receipt::Receipt;
pub use trace::{Balance, Trace};
pub use transaction::{Fee, Movement, Transaction};
pub use withdrawal::Withdrawal;

use crate::native::{Error, Network};

/// One parsed block: its reference and transactions in block order.
pub type Block = base::Block<Transaction>;
/// Transactions of one block in block order, with unique ids.
pub type Transactions = base::Transactions<Transaction>;

/// Exact source evidence for one block, retained for replay and reorg recovery.
///
/// The payload is the JSON-RPC `result` bytes of `eth_getBlockByNumber` (with
/// full transactions), of each transaction receipt in block order, and of the
/// block's `debug_traceBlockByHash` prestate diff, plus the network they were
/// read from: nothing in the bytes names it, and EVM chains share block
/// formats, so it is stated by the client that verified the chain ID. Keeping
/// bytes rather than parsed facts makes replay independent of this parser.
///
/// Traces are an order of magnitude larger than the block that carries them
/// and are held for one indexing pass only: nothing writes a `RawBlock` to
/// storage, and no raw-block archive exists to write one to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawBlock {
    reference: BlockRef,
    network: Network,
    block: Vec<u8>,
    receipts: Vec<Vec<u8>>,
    traces: Vec<u8>,
}

impl RawBlock {
    /// Pairs evidence with the reference it claims; [`Self::parse`] checks both agree.
    #[must_use]
    pub const fn new(
        reference: BlockRef,
        network: Network,
        block: Vec<u8>,
        receipts: Vec<Vec<u8>>,
        traces: Vec<u8>,
    ) -> Self {
        Self {
            reference,
            network,
            block,
            receipts,
            traces,
        }
    }

    /// The block this evidence claims to be.
    #[must_use]
    pub const fn reference(&self) -> &BlockRef {
        &self.reference
    }

    /// The network the evidence was read from.
    #[must_use]
    pub const fn network(&self) -> &Network {
        &self.network
    }

    /// The `eth_getBlockByNumber` result bytes.
    #[must_use]
    pub fn block(&self) -> &[u8] {
        &self.block
    }

    /// One receipt result per transaction, in block order.
    #[must_use]
    pub fn receipts(&self) -> &[Vec<u8>] {
        &self.receipts
    }

    /// The `debug_traceBlockByHash` result bytes: one prestate diff per
    /// transaction, in block order.
    #[must_use]
    pub fn traces(&self) -> &[u8] {
        &self.traces
    }

    /// Every consensus-layer credit this block makes, in block order.
    ///
    /// These are the block's value changes that **no transaction names**, so
    /// they are not in [`Self::parse`]'s result and cannot be: a [`Block`] is
    /// its transactions. An account that only ever received withdrawals has
    /// an empty history and a non-zero balance, which is why this is a
    /// balance fact independent of transaction history.
    ///
    /// It reads the payload already held — no RPC call, and nothing of the
    /// transaction path.
    ///
    /// # Errors
    ///
    /// [`crate::native::ErrorKind::InvalidBlock`] naming the first violation.
    pub fn withdrawals(&self) -> Result<Vec<Withdrawal>, Error> {
        Withdrawal::parse_all(&self.block)
    }

    /// Validates the evidence and decodes it into a [`Block`].
    ///
    /// # Errors
    ///
    /// [`crate::native::ErrorKind::InvalidBlock`] naming the first violation.
    pub fn parse(&self) -> Result<Block, Error> {
        let head = Head::parse(&self.block, Some(self.reference.height), true)?;
        if head.reference != self.reference {
            return Err(Error::invalid_block(
                "retained Ethereum block reference does not match its raw payload",
            ));
        }
        let receipts = Receipt::parse_all(&self.receipts, &head)?;
        let traces = Trace::parse_all(&self.traces, &head)?;
        let transactions = head
            .transactions
            .into_iter()
            .zip(receipts)
            .zip(traces)
            .map(|((listed, receipt), trace)| Transaction::new(listed, receipt, trace))
            .collect::<Result<Vec<_>, _>>()?;
        let transactions = Transactions::new(transactions).map_err(|_| {
            Error::invalid_block("Ethereum block contains duplicate transaction hashes")
        })?;
        Ok(Block::new(&self.network, head.reference, transactions))
    }
}
