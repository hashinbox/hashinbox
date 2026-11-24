//! `base::BlockStore` on the `block`, `wallet_index`, `transaction_index`
//! and `leg` models.

use base::{BlockHeader, BlockStore, Blocks, Future, Kept, Network};

use super::Stores;
use crate::models::{block, int};
use crate::usecase::blocks;

impl BlockStore for Stores {
    /// One write ([`blocks::keep`]); once it commits, each block is logged
    /// as `index.block` (`position transactions matched legs`: debug, info
    /// when it matched), each leg kept as `index.leg` (`wallet asset kind
    /// amount txid position total`, info), and each watched transaction
    /// found as `index.found` (`txid position reverted`, info).
    fn insert<'a>(&'a self, blocks: &'a Blocks) -> Future<'a, anyhow::Result<Kept>> {
        Box::pin(async move {
            let run = blocks.to_vec();
            let kept = self
                .db
                .write(move |connection| blocks::keep(connection, &run))
                .await?;
            let mut summary = Kept::default();
            for block in &kept {
                let (position, transactions, matched) =
                    (block.position, block.transactions, block.matched);
                let legs = block.legs.len();
                if matched > 0 {
                    tracing::info!(target: "index.block", position, transactions, matched, legs, "Block kept");
                } else {
                    tracing::debug!(target: "index.block", position, transactions, matched, legs, "Block kept");
                }
                for leg in block.legs.iter() {
                    let txid = leg.transaction.as_ref().map_or("-", |id| id.txid.as_str());
                    let total = leg.total.as_deref().unwrap_or("-");
                    tracing::info!(
                        target: "index.leg",
                        wallet = %leg.address, asset = %leg.asset, kind = leg.kind_name(),
                        amount = %leg.amount, txid, position, total,
                        "Wallet leg kept"
                    );
                }
                for (txid, reverted) in &block.found {
                    let (txid, reverted) = (txid.as_str(), *reverted);
                    tracing::info!(target: "index.found", txid, position, reverted, "Watched transaction found");
                }
                summary.matched += matched as u64;
                summary.legs += legs as u64;
            }
            Ok(summary)
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
