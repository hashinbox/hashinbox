//! `base::BlockStore` on the `block`, `wallet_index` and `transaction_index`
//! models.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure};
use base::{Block as BaseBlock, BlockHeader, BlockStore, Blocks, Future, Network, Networked as _};
use diesel::SqliteConnection;

use super::Stores;
use crate::models::{
    Block,
    block::{self, Place},
    int, transaction_index, wallet_index,
};

impl BlockStore for Stores {
    fn insert<'a>(&'a self, blocks: &'a Blocks) -> Future<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let network = blocks.network().clone();
            let run = blocks.to_vec();
            self.db
                .write(move |connection| keep(connection, &network, &run))
                .await?;
            Ok(())
        })
    }

    fn find_last<'a>(
        &'a self,
        network: &'a Network,
    ) -> Future<'a, anyhow::Result<Option<BlockHeader>>> {
        let network = network.clone();
        Box::pin(self.db.read(move |connection| {
            block::db::find_below(connection, &network, i64::MAX)?
                .map(|block| BlockHeader::try_from(&block))
                .transpose()
        }))
    }

    fn delete<'a>(&'a self, network: &'a Network, from: u64) -> Future<'a, anyhow::Result<()>> {
        let network = network.clone();
        Box::pin(async move {
            let from = int(from)?;
            let now = int(u64::try_from(
                SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
            )?)?;
            self.db
                .write(move |connection| {
                    let below = block::db::find_below(connection, &network, from)?;
                    wallet_index::db::rewind(connection, &network, from, below.as_ref())?;
                    transaction_index::db::rewind(connection, &network, from, below.as_ref())?;
                    block::db::reorg(connection, &network, from, now)?;
                    Ok(())
                })
                .await
        })
    }
}

/// Keeps `run` ([`BlockStore::insert`]): each block with its facts, checked
/// against the live blocks around the run, read once and kept current as
/// the run is written. A block conflicting with a kept one, or added
/// without linking to an adjacent live neighbour (one at or above its
/// parent's position below it, one whose parent is at or below its position
/// above it), fails the write. A neighbour further away is a gap (pruned
/// blocks) and is allowed: an index stays contiguous because it only moves
/// onto a block whose parent is its `indexed_at` (`wallet_index::db::advance`).
fn keep(
    connection: &mut SqliteConnection,
    network: &Network,
    run: &[BaseBlock],
) -> anyhow::Result<()> {
    let positions = run
        .iter()
        .map(|base| int(base.header.position))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let (Some(&lowest), Some(&highest)) = (positions.iter().min(), positions.iter().max()) else {
        return Ok(());
    };
    let mut live = block::db::find_around(connection, network, lowest, highest)?;
    let mut added = Vec::new();
    for (base, position) in run.iter().zip(positions) {
        ensure!(base.network == *network, "a block is not of {network:?}");
        match live.place(&base.header)? {
            Place::Held(_) => {}
            Place::Taken => bail!("{network:?}: block {position} conflicts with the kept one"),
            Place::Free => added.push(position),
        }
        let mut block = Block::try_from(base)?;
        block::db::save(connection, &mut block)?;
        live.insert(block);
    }
    added
        .into_iter()
        .try_for_each(|position| live.check_links(position))
}

/// Keeps `header`, a block of `network`, live with no facts, moving no index
/// (a settle's block): the kept one at its position when that is it, linked
/// as [`keep`] links a block. `None`: another live block is there.
pub(super) fn keep_empty(
    connection: &mut SqliteConnection,
    network: &Network,
    header: &BlockHeader,
) -> anyhow::Result<Option<Block>> {
    let position = int(header.position)?;
    let mut live = block::db::find_around(connection, network, position, position)?;
    match live.place(header)? {
        Place::Held(held) => return Ok(Some(held.clone())),
        Place::Taken => return Ok(None),
        Place::Free => {}
    }
    let mut block = Block::empty(network, header)?;
    block::db::save_row(connection, &mut block)?;
    live.insert(block.clone());
    live.check_links(position)?;
    Ok(Some(block))
}
