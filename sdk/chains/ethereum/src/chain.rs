//! [`Chain`]: Ethereum blocks as `base` blocks, final once finalized.

use anyhow::{Context as _, ensure};
use base::{
    Addressed as _, Asset, Balances, Block, Blocks, Chained, Dated, Future, Holding, Network,
    Networked, Tips,
};

use crate::{Address, Client, Tokens};

/// One Ethereum network read from its node. Position and height are both the
/// block number; a block is final once at or below the node's `finalized`
/// block. Legs are of ether and of the allowlisted tokens only; ether a
/// contract moves internally is not seen (standard RPC records it nowhere).
pub struct Chain {
    rpc: Client,
    network: Network,
    tokens: Tokens,
}

impl Chain {
    /// Reads over `rpc`, indexing `tokens`.
    #[must_use]
    pub fn new(rpc: Client, tokens: Tokens) -> Self {
        let network = rpc.network().into();
        Self {
            rpc,
            network,
            tokens,
        }
    }
}

impl Networked for Chain {
    fn network(&self) -> &Network {
        &self.network
    }
}

impl Chained for Chain {
    fn tips(&self) -> Future<'_, anyhow::Result<Tips>> {
        Box::pin(self.rpc.tips())
    }

    /// Ethereum skips no height: a block the node does not hold yet is an
    /// error, not `None`.
    fn block(&self, position: u64) -> Future<'_, anyhow::Result<Option<Block>>> {
        Box::pin(async move {
            let block = self.blocks(position, position).await?.first().cloned();
            anyhow::ensure!(
                block.is_some(),
                "Ethereum block {position} is not available yet"
            );
            Ok(block)
        })
    }

    fn blocks(&self, start: u64, end: u64) -> Future<'_, anyhow::Result<Blocks>> {
        Box::pin(self.rpc.blocks(start, end, &self.tokens))
    }
}

/// Finds blocks by time through header-only reads.
impl Dated for Chain {
    fn time(&self, position: u64) -> Future<'_, anyhow::Result<Option<u64>>> {
        Box::pin(async move { Ok(Some(self.rpc.time(position).await?)) })
    }
}

/// Ether by `eth_getBalance`, a token by its `balanceOf`; both at the
/// node's finalized block.
impl Balances for Chain {
    /// `None` while the finalized block is below `at`.
    fn balance<'a>(
        &'a self,
        address: &'a base::Address,
        asset: &'a Asset,
        at: u64,
    ) -> Future<'a, anyhow::Result<Option<Holding>>> {
        Box::pin(async move {
            let owner = Address::parse(&address.value, self.rpc.network())?;
            ensure!(
                owner.address() == address && asset.network == self.network,
                "not an address or asset of this Ethereum network"
            );
            let token = if asset.is_native() {
                None
            } else {
                Some(self.tokens.get(asset).context("not an allowlisted token")?)
            };
            let position = self.rpc.tips().await?.confirmed;
            if position < at {
                return Ok(None);
            }
            let amount = self
                .rpc
                .balance(owner.as_native(), token.as_ref(), position)
                .await?;
            Ok(Some(Holding { position, amount }))
        })
    }
}
