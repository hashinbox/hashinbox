//! An address's mined transactions with their legs, over live blocks, on
//! the `leg`, `transaction` and `block` models. Each read is one snapshot.
//!
//! The store records no confirmation: these answer every live block, up to
//! the produced tip, and the caller states how far that is.

use anyhow::Context as _;
use base::{Address, Asset, BlockHeader};

use crate::{
    db::Db,
    models::{block, int, leg, transaction},
};

/// Where a history page starts: after this transaction's
/// (block position, order in block, txid).
pub(crate) type After = (u64, u64, String);

/// At most `limit` transactions of `address` in `asset` in live blocks,
/// after `after`, in chain order, each with its block and only the legs
/// of `address` in `asset`.
///
/// # Errors
///
/// The read, or a stored value that does not decode.
pub(crate) async fn history(
    db: &Db,
    address: &Address,
    asset: &Asset,
    after: Option<After>,
    limit: usize,
) -> anyhow::Result<Vec<(BlockHeader, base::Transaction)>> {
    let (address, asset) = (address.clone(), asset.clone());
    let limit = i64::try_from(limit).context("history limit is out of range")?;
    let after = match after {
        Some((position, order, txid)) => (int(position)?, int(order)?, txid),
        None => (-1, -1, String::new()),
    };
    db.read(move |connection| {
        let key = (after.0, after.1, after.2.as_str());
        let rows = transaction::db::find_page(connection, &address, &asset, key, limit)?;
        let mut page = Vec::with_capacity(rows.len());
        for row in rows {
            let block = row
                .block
                .as_deref()
                .context("a paged transaction is pending")?;
            let header = BlockHeader::try_from(&block::db::get_by_id(connection, block)?)?;
            let legs = leg::db::find_for(connection, &row.id)?
                .into_iter()
                .filter(|leg| leg.address == address.value && leg.asset == asset.id)
                .collect();
            page.push((header, row.transaction(legs)?));
        }
        Ok(page)
    })
    .await
}
