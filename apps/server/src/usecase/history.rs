//! An address's mined transactions with their legs, over live blocks, on
//! the `transaction` model. Each read is one snapshot.
//!
//! The store records no confirmation: these answer every live block, up to
//! the produced tip, and the caller states how far that is.

use anyhow::Context as _;
use base::{Address, Asset};

use crate::{
    db::Db,
    models::{int, transaction, transaction::Transaction},
};

/// Where a history page starts: after this transaction's
/// (block position, order in block, txid).
pub(crate) type After = (u64, u64, String);

/// At most `limit` transactions of `address` in `asset` in live blocks,
/// after `after`, in chain order, each with only the legs of `address` in
/// `asset`.
///
/// # Errors
///
/// The read.
pub(crate) async fn history(
    db: &Db,
    address: &Address,
    asset: &Asset,
    after: Option<After>,
    limit: usize,
) -> anyhow::Result<Vec<Transaction>> {
    let (address, asset) = (address.clone(), asset.clone());
    let limit = i64::try_from(limit).context("history limit is out of range")?;
    let after = match after {
        Some((position, order, txid)) => (int(position)?, int(order)?, txid),
        None => (-1, -1, String::new()),
    };
    db.read(move |connection| {
        let key = (after.0, after.1, after.2.as_str());
        Ok(transaction::db::find_page(
            connection, &address, &asset, key, limit,
        )?)
    })
    .await
}
