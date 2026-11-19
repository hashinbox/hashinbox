//! [`Chain`]: Solana blocks as `base` blocks, final once rooted.

use anyhow::ensure;
use base::{
    Addressed as _, Asset, Balances, Block, Blocks, Chained, Confirmed, Confirmer, Dated, Future,
    Holding, Line, Network, Networked, Tips, TransactionId,
};

use crate::{Address, Client, Tokens};

/// Slots below the tip read by default ([`Chain::with_read_depth`]): about
/// 7 minutes.
pub const READ_DEPTH: u64 = 1_000;

/// One Solana cluster read from its node at confirmed commitment. Position
/// is the slot (skipped slots hold no block), height the block height; a
/// block is final once rooted (`finalized`). Legs are of SOL and of the
/// allowlisted tokens only.
pub struct Chain {
    rpc: Client,
    network: Network,
    tokens: Tokens,
    read_depth: u64,
}

impl Chain {
    /// Reads over `rpc`, indexing `tokens`, [`READ_DEPTH`] slots deep.
    #[must_use]
    pub fn new(rpc: Client, tokens: Tokens) -> Self {
        let network = rpc.network().into();
        Self {
            rpc,
            network,
            tokens,
            read_depth: READ_DEPTH,
        }
    }

    /// Reads nothing more than `read_depth` slots below the tip: an index
    /// further behind settles.
    #[must_use]
    pub const fn with_read_depth(mut self, read_depth: u64) -> Self {
        self.read_depth = read_depth;
        self
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

    /// The tip less `read_depth`, not the block after the final one: the
    /// node's finalized slot is its tip, so final blocks are read too. An
    /// index behind it settles at the chain's balance.
    fn line(&self) -> Line {
        Line::Depth(self.read_depth)
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
            let mint = self.tokens.contract(asset)?;
            self.rpc.balance(owner.as_native(), mint.as_ref(), at).await
        })
    }
}

/// Mined by its signature's status (history searched), judged at the
/// node's finalized slot; reverted by its `err`.
impl Confirmer for Chain {
    /// `None` while the finalized slot is below `at`; found only once
    /// finalized, at or below it.
    fn confirm<'a>(
        &'a self,
        id: &'a TransactionId,
        at: u64,
    ) -> Future<'a, anyhow::Result<Option<Confirmed>>> {
        Box::pin(async move {
            ensure!(
                id.network == self.network,
                "not a transaction of this Solana network"
            );
            let position = self.rpc.tips().await?.confirmed;
            if position < at {
                return Ok(None);
            }
            let mined = self.rpc.finalized_in(&id.id).await?;
            let mined = mined.filter(|(slot, _)| *slot <= position);
            Ok(Some(Confirmed {
                position,
                found: mined.map(|(slot, _)| slot),
                reverted: mined.is_some_and(|(_, failed)| failed),
            }))
        })
    }
}
