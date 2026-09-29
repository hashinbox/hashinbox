use base::{BoxFuture, Signer};

use super::{SignedTransaction, Transfer, UnsignedTransaction};
use crate::native::{Address, Error, Wei};

/// Node-observed facts a transfer needs before it can be signed.
///
/// Produced by [`super::Preflight`]; the nonce is reserved by the caller
/// (the SDK's send pipeline keeps one per sender, see [`super::Nonces`]).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BuildContext {
    /// The chain the transaction signs for.
    pub chain_id: u64,
    /// The sender: the signature must recover to it.
    pub from: Address,
    /// The sender's reserved nonce.
    pub nonce: u64,
    /// Estimated gas plus the configured margin.
    pub gas_limit: u64,
    /// `2 * base fee + priority fee`, within the ceiling.
    pub max_fee_per_gas: Wei,
    /// The node's suggested tip, within the ceiling.
    pub max_priority_fee_per_gas: Wei,
}

/// Builds and signs one EIP-1559 transaction from a [`Transfer`] and its
/// [`BuildContext`]. Pure: no RPC.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Builder {
    transfer: Transfer,
    context: BuildContext,
}

impl Builder {
    /// Pairs a transfer with its node-observed context.
    #[must_use]
    pub const fn new(transfer: Transfer, context: BuildContext) -> Self {
        Self { transfer, context }
    }

    /// The unsigned transaction.
    ///
    /// # Errors
    ///
    /// When the context has a zero chain ID or gas limit, or inverted fees.
    pub fn build(&self) -> Result<UnsignedTransaction, Error> {
        UnsignedTransaction::new(&self.transfer, &self.context)
    }

    /// Builds, asks `signer` for a recoverable signature, and verifies that it
    /// recovers to the context's sender.
    pub fn sign<'a>(
        &'a self,
        signer: &'a dyn Signer,
    ) -> BoxFuture<'a, Result<SignedTransaction, Error>> {
        Box::pin(async move { self.build()?.sign(signer).await })
    }
}
