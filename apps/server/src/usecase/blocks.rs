//! Kept blocks over the `block`, `wallet_index`, `transaction_index`,
//! `transaction` and `leg` models, its legs valued by the `network_asset`
//! model and the market candles in memory: each model writes its own
//! table, the order lives here.

use base::{Block as BaseBlock, Network};
use diesel::SqliteConnection;

use crate::models::{
    Block, block, leg, market_candle_1h::MarketCandles1hExchangeAssetMapCached, network_asset,
    transaction, transaction_index, wallet_index,
};

/// One block as [`keep`] kept it: what its events (`index.block`,
/// `index.leg`, `index.found`) tell once the write commits.
pub(crate) struct Kept {
    /// Its position.
    pub(crate) position: i64,
    /// Its transactions, as fetched.
    pub(crate) transactions: usize,
    /// Its transactions kept: with a leg of a wallet index, or watched.
    pub(crate) matched: usize,
    /// Its legs kept, each with its index's total after it.
    pub(crate) legs: leg::Legs,
    /// The watched transactions found in it: txid, and whether it reverted.
    pub(crate) found: Vec<(String, bool)>,
}

/// Keeps `run` (one network, linked): each block's row, then applies it
/// ([`apply`]), its legs valued by `candles`; what each
/// block kept. A block conflicting with a kept one at
/// its position fails the write ([`block::db::save`]); links need no check:
/// the run is linked, and the indexer reads forward from the last kept
/// block.
///
/// # Errors
///
/// When a block does not decode or a write fails.
pub(crate) fn keep(
    connection: &mut SqliteConnection,
    run: &[BaseBlock],
    candles: &MarketCandles1hExchangeAssetMapCached,
) -> anyhow::Result<Vec<Kept>> {
    let mut kept = Vec::with_capacity(run.len());
    for base in run {
        let mut block = Block::try_from(base)?;
        block::db::save(connection, &mut block)?;
        kept.push(apply(connection, block, candles)?);
    }
    Ok(kept)
}

/// Applies kept `block` to the indexes, in 072's order:
///
/// 1. the wallet and transaction indexes it applies to move onto it, and
///    it keeps only the wallet indexes' legs and the transactions found,
///    with their legs of the wallet indexes behind it too ([`Block::keep`]);
/// 2. its legs of the wallet indexes its legs are of are deleted (a block
///    kept again writes them again);
/// 3. each transaction's row is upserted and its legs of no block or
///    another one (pending, or mined before a reorg) deleted; then the legs
///    are valued at their assets' latest prices in `candles`
///    ([`leg::Legs::price`]) and appended to their indexes'
///    ([`leg::db::append`]).
///
/// # Errors
///
/// When a value does not decode or a write fails (a seq an index holds).
fn apply(
    connection: &mut SqliteConnection,
    mut block: Block,
    candles: &MarketCandles1hExchangeAssetMapCached,
) -> anyhow::Result<Kept> {
    let transactions = block.transactions.len();
    let ours = wallet_index::db::advance(connection, &block)?;
    transaction_index::db::set_indexed_at(connection, &block)?;
    let watched = transaction_index::db::set_found_at(connection, &block, &block.txids())?;
    let senders = block.indexes_of(&watched);
    let behind = wallet_index::db::find_behind_block(connection, &block, &senders)?;
    block.keep(&ours, &watched, &behind);
    let found = block.transactions.iter();
    let found = found.filter(|transaction| watched.contains(transaction.id.as_ref()));
    let found = found.map(|transaction| (transaction.txid.clone(), transaction.state == 2));

    let found = found.collect();
    let mut legs = block.take_legs();
    leg::db::delete_in(connection, &block.id, block.position, &legs.indexes())?;
    for transaction in &block.transactions {
        transaction::db::upsert(connection, transaction)?;
        leg::db::delete_moved(connection, &block.id, &transaction.id)?;
    }
    let assets = network_asset::db::find_of_network(connection, &block.chain, &block.network)?;
    legs.price(&assets, candles)?;
    leg::db::append(connection, &mut legs, block.position)?;
    Ok(Kept {
        position: block.position,
        transactions,
        matched: block.transactions.len(),
        legs,
        found,
    })
}

/// Deletes `network`'s blocks from position `from` up (a reorg, design 088):
/// rows first, each model its own, so a block holds only its rewards once
/// it goes: others' transactions gone with their legs, adjustments gone (no
/// block to go with), then ours (`broadcasted_at`) and their legs pending;
/// the indexes back below `from`.
///
/// # Errors
///
/// When a write fails.
pub(crate) fn delete_from(
    connection: &mut SqliteConnection,
    network: &Network,
    from: i64,
) -> anyhow::Result<()> {
    let to = from - 1;
    transaction::db::delete_unbroadcast_from(connection, network, from)?;
    leg::db::delete_kind_from(connection, network, from, leg::ADJUSTMENT)?;
    leg::db::set_pending_from(connection, network, from)?;
    transaction::db::set_pending_from(connection, network, from)?;
    block::db::delete_from(connection, network, from)?;
    wallet_index::db::cap_indexed_at(connection, network, to)?;
    transaction_index::db::cap_indexed_at(connection, network, to)?;
    transaction_index::db::clear_found_from(connection, network, from)?;
    Ok(())
}
