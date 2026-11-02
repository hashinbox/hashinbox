//! `base::BlockStore` on the `block`, `transaction`,
//! `leg` and `wallet_index` models.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, ensure};
use base::Addressed as _;
use base::{Block as BaseBlock, BlockHeader, BlockStore, Blocks, Future, Network, Networked as _};
use diesel::SqliteConnection;

use super::Stores;
use crate::models::{Leg, block, int, leg, transaction, wallet_index};

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

/// Keeps `run` ([`BlockStore::insert`]); the headers of the blocks it added
/// or kept facts or progress for.
fn keep(
    connection: &mut SqliteConnection,
    network: &Network,
    run: &[BaseBlock],
) -> anyhow::Result<Vec<BlockHeader>> {
    let mut kept = Vec::new();
    let mut added = Vec::new();
    for block in run {
        ensure!(block.network == *network, "a block is not of {network:?}");
        let header = &block.header;
        let position = int(header.position)?;
        let id = match block::db::find_live(connection, network, position)? {
            Some(held) if held.is(header) => held.id.clone(),
            Some(_) => bail!("{network:?}: block {position} conflicts with the kept one"),
            None => {
                let id = block::db::insert(connection, network, header)?;
                added.push(position);
                kept.push(header.clone());
                id
            }
        };
        let parent = block::db::find_below(connection, network, position)?
            .filter(|below| below.is_parent_of(header))
            .map(|below| below.id);
        let pairs =
            wallet_index::db::find_applying(connection, network, position, parent.as_deref())?;
        if pairs.is_empty() {
            continue;
        }
        for transaction in &block.transactions {
            let legs = (0_u32..)
                .zip(&transaction.legs)
                .filter(|(_, leg)| applies(&pairs, leg.address(), leg.asset()));
            let legs: Vec<_> = legs.collect();
            if legs.is_empty() {
                continue;
            }
            transaction::db::upsert_mined(connection, &id, transaction, legs)?;
        }
        let rewards = block
            .rewards
            .iter()
            .filter(|reward| applies(&pairs, &reward.address, &reward.asset))
            .map(|reward| Leg::reward(&id, reward))
            .collect::<anyhow::Result<leg::Legs>>()?;
        leg::db::insert(connection, &rewards)?;
        wallet_index::db::advance(connection, network, &pairs, &id)?;
        if kept.last() != Some(header) {
            kept.push(header.clone());
        }
    }
    // Every added block must link to its live neighbours.
    for position in added {
        let Some(block) = block::db::find_live(connection, network, position)? else {
            continue;
        };
        let header = BlockHeader::try_from(&block)?;
        if let Some(below) = block::db::find_below(connection, network, position)?
            && !below.is_parent_of(&header)
        {
            bail!("{network:?}: block {position} does not link onto the kept block below it");
        }
        if let Some(above) = block::db::find_above(connection, network, position)?
            && !block.is_parent_of(&BlockHeader::try_from(&above)?)
        {
            bail!("{network:?}: the kept block above {position} does not link onto it");
        }
    }
    Ok(kept)
}

/// Whether (`address`, `asset`) is one of `pairs`.
fn applies(
    pairs: &std::collections::BTreeSet<(String, String)>,
    address: &base::Address,
    asset: &base::Asset,
) -> bool {
    pairs.contains(&(address.value.clone(), asset.id.clone()))
}
