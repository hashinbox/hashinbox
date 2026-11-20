//! [`Chain`]: Ethereum blocks as `base` blocks, confirmed `confirmations`
//! below the tip.

use alloy_primitives::B256;
use anyhow::{Context as _, ensure};
use base::{
    Addressed as _, Asset, Balances, Block, Blocks, Chained, Confirmed, Confirmer, Dated, Future,
    Holding, Line, Network, Networked, Tips, TransactionId,
};

use crate::{Address, Client, Tokens};

/// One Ethereum network read from its node. Position and height are both the
/// block number; a block is confirmed once `confirmations` below the tip. Legs are of ether and of the allowlisted tokens only; ether a
/// contract moves internally is not seen (standard RPC records it nowhere).
pub struct Chain {
    rpc: Client,
    network: Network,
    tokens: Tokens,
    confirmations: u64,
}

impl Chain {
    /// Reads over `rpc`, indexing `tokens`, a block confirmed
    /// `confirmations` below the tip.
    #[must_use]
    pub fn new(rpc: Client, tokens: Tokens, confirmations: u64) -> Self {
        let network = rpc.network().into();
        Self {
            rpc,
            network,
            tokens,
            confirmations,
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

    fn confirmations(&self) -> u64 {
        self.confirmations
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
        Box::pin(
            self.rpc
                .blocks(start, end, &self.tokens, self.confirmations),
        )
    }

    /// The block after the confirmed one: an index behind it settles at the
    /// chain's balance.
    fn line(&self) -> Line {
        Line::Final
    }
}

/// Finds blocks by time through header-only reads.
impl Dated for Chain {
    fn time(&self, position: u64) -> Future<'_, anyhow::Result<Option<u64>>> {
        Box::pin(async move { Ok(Some(self.rpc.time(position).await?)) })
    }
}

/// Ether by `eth_getBalance`, a token by its `balanceOf`; both at the
/// confirmed block.
impl Balances for Chain {
    /// `None` while the confirmed block is below `at`.
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
            let token = self.tokens.contract(asset)?;
            let position = self.rpc.tips().await?.confirmed(self.confirmations);
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

/// Mined by its receipt, judged at the confirmed block; reverted by its
/// status.
impl Confirmer for Chain {
    /// `None` while the confirmed block is below `at`; found only at or
    /// below it, confirmed.
    fn confirm<'a>(
        &'a self,
        id: &'a TransactionId,
        at: u64,
    ) -> Future<'a, anyhow::Result<Option<Confirmed>>> {
        Box::pin(async move {
            ensure!(
                id.network == self.network,
                "not a transaction of this Ethereum network"
            );
            let hash: B256 = id.id.parse().context("not an Ethereum transaction id")?;
            let position = self.rpc.tips().await?.confirmed(self.confirmations);
            if position < at {
                return Ok(None);
            }
            let mined = self.rpc.mined_in(hash).await?;
            let mined = mined.filter(|(block, _)| *block <= position);
            Ok(Some(Confirmed {
                position,
                found: mined.map(|(block, _)| block),
                reverted: mined.is_some_and(|(_, reverted)| reverted),
            }))
        })
    }
}
