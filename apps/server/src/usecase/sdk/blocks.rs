//! `base::BlockStore` on the `block` and `wallet_index` models.

use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, ensure};
use base::{Block as BaseBlock, BlockHeader, BlockStore, Blocks, Future, Network, Networked as _};
use diesel::SqliteConnection;

use super::Stores;
use crate::models::{Block, block, int, wallet_index};

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
            block::db::find_last(connection, &network)?
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
                    block::db::archive(connection, &network, from, now)?;
                    Ok(())
                })
                .await
        })
    }
}

/// Keeps `run` ([`BlockStore::insert`]): each block with its facts, checked
/// against the live blocks around the run, read once and kept current as
/// the run is written. A block conflicting with a kept one, or added
/// without linking to its live neighbours, fails the write: blocks are
/// never skipped, a gap fails loudly.
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
    let mut live: BTreeMap<i64, Block> =
        block::db::find_around(connection, network, lowest, highest)?
            .into_iter()
            .map(|block| (block.position, block))
            .collect();
    let mut added = Vec::new();
    for (base, position) in run.iter().zip(positions) {
        ensure!(base.network == *network, "a block is not of {network:?}");
        match live.get(&position) {
            Some(held) if held.is(&base.header) => {}
            Some(_) => bail!("{network:?}: block {position} conflicts with the kept one"),
            None => added.push(position),
        }
        let mut block = Block::try_from(base)?;
        block::db::save(connection, &mut block)?;
        live.insert(position, block);
    }
    // Every added block must link to its live neighbours.
    for position in added {
        let Some(block) = live.get(&position) else {
            continue;
        };
        let header = BlockHeader::try_from(block)?;
        if let Some((_, below)) = live.range(..position).next_back()
            && !below.is_parent_of(&header)
        {
            bail!("{network:?}: block {position} does not link onto the kept block below it");
        }
        if let Some((_, above)) = live.range(position + 1..).next()
            && !block.is_parent_of(&BlockHeader::try_from(above)?)
        {
            bail!("{network:?}: the kept block above {position} does not link onto it");
        }
    }
    Ok(())
}
