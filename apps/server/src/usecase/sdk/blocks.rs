//! `base::BlockStore` on the `block`, `wallet_index`, `transaction_index`
//! and `leg` models.

use base::{BlockHeader, BlockStore, Blocks, Future, Network};

use super::Stores;
use crate::models::{block, int};
use crate::usecase::blocks;

impl BlockStore for Stores {
    fn insert<'a>(&'a self, blocks: &'a Blocks) -> Future<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let run = blocks.to_vec();
            self.db
                .write(move |connection| blocks::keep(connection, &run))
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
            self.db
                .write(move |connection| blocks::delete_from(connection, &network, from))
                .await
        })
    }
}
