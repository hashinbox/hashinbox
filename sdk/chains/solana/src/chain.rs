//! [`Chain`]: Solana blocks as `base` blocks, final once rooted.

use anyhow::{Context as _, ensure};
use base::{
    Addressed as _, Asset, Balances, Block, Blocks, Chained, Dated, Future, Holding, Network,
    Networked, Tips,
};

use crate::{Address, Client, Tokens};

/// One Solana cluster read from its node at confirmed commitment. Position
/// is the slot (skipped slots hold no block), height the block height; a
/// block is final once rooted (`finalized`). Legs are of SOL and of the
/// allowlisted tokens only.
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

    /// `None` for a skipped slot.
    fn block(&self, position: u64) -> Future<'_, anyhow::Result<Option<Block>>> {
        Box::pin(async move { Ok(self.blocks(position, position).await?.first().cloned()) })
    }

    fn blocks(&self, start: u64, end: u64) -> Future<'_, anyhow::Result<Blocks>> {
        Box::pin(self.rpc.blocks(start, end, &self.tokens))
    }
}

/// Finds blocks by time through header-only reads.
impl Dated for Chain {
    fn time(&self, position: u64) -> Future<'_, anyhow::Result<Option<u64>>> {
        Box::pin(self.rpc.time(position))
    }
}

/// SOL by `getBalance`, a token by the sum of the owner's accounts of its
/// mint; both finalized, at the slot the node answers at.
impl Balances for Chain {
    /// `None` while the node's finalized slot is below `at`.
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
                "not an address or asset of this Solana network"
            );
            let mint = if asset.is_native() {
                None
            } else {
                let token = self.tokens.get(asset).context("not an allowlisted token")?;
                Some(token.mint)
            };
            self.rpc.balance(owner.as_native(), mint.as_ref(), at).await
        })
    }
}
