//! `WalletIndexStore` on the `wallet_index` model; an index's wallet is kept
//! with it (watch-only when its key is not kept). A settle and a restart
//! write on the `block`, `leg`, `transaction` and `transaction_forgotten` models too.

use std::collections::BTreeSet;

use anyhow::Context as _;
use base::{
    BlockHeader, Decimal, Future, Holding, IndexState, IntoIndex, Network, Networked as _,
    WalletIndex, WalletIndexStore,
};

use diesel::SqliteConnection;

use super::{Stores, blocks, transaction_indexes};
use crate::models::{
    Leg, Wallet, block, int, leg, transaction, transaction_forgotten::Why, uint, wallet,
    wallet_index, wallet_index::WalletIndexId,
};

impl WalletIndexStore for Stores {
    /// The index first (the chain's reads), then one write: its wallet and
    /// the index.
    fn insert<'a>(
        &'a self,
        index: &'a dyn IntoIndex<WalletIndex>,
    ) -> Future<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let index = index.index().await?;
            self.db
                .write(move |connection| {
                    wallet::db::insert(connection, &Wallet::from_address(&index.address)?)?;
                    wallet_index::db::insert(connection, &index)
                })
                .await
        })
    }

    fn find_next<'a>(
        &'a self,
        network: &'a Network,
        from: u64,
        through: Option<u64>,
    ) -> Future<'a, anyhow::Result<Option<u64>>> {
        let network = network.clone();
        Box::pin(async move {
            let from = int(from)?;
            // Above SQLite's range bounds nothing.
            let through = through.map(|through| int(through).unwrap_or(i64::MAX));
            self.db
                .read(move |connection| {
                    wallet_index::db::find_next(connection, &network, from, through)?
                        .map(uint)
                        .transpose()
                })
                .await
        })
    }

    fn find_all<'a>(
        &'a self,
        network: &'a Network,
    ) -> Future<'a, anyhow::Result<Vec<WalletIndex>>> {
        let network = network.clone();
        Box::pin(
            self.db
                .read(move |connection| wallet_index::db::find_all(connection, &network)),
        )
    }

    fn find_behind<'a>(
        &'a self,
        network: &'a Network,
        below: u64,
        limit: usize,
    ) -> Future<'a, anyhow::Result<Vec<WalletIndex>>> {
        let network = network.clone();
        // Above SQLite's range: every index is below it; take them all.
        let below = int(below).unwrap_or(i64::MAX);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        Box::pin(self.db.read(move |connection| {
            wallet_index::db::find_behind(connection, &network, below, limit)
        }))
    }

    fn move_start<'a>(
        &'a self,
        index: &'a WalletIndex,
        start: u64,
    ) -> Future<'a, anyhow::Result<()>> {
        let index = index.clone();
        Box::pin(
            self.db
                .write(move |connection| wallet_index::db::move_start(connection, &index, start)),
        )
    }

    fn set_state<'a>(
        &'a self,
        index: &'a WalletIndex,
        state: IndexState,
        reason: Option<&'a str>,
    ) -> Future<'a, anyhow::Result<()>> {
        let (index, reason) = (index.clone(), reason.map(str::to_owned));
        Box::pin(self.db.write(move |connection| {
            wallet_index::db::set_state(
                connection,
                &index.address,
                Some(&index.asset.id),
                state,
                reason.as_deref(),
            )?;
            Ok(())
        }))
    }
    /// One write: block `S` kept unless it is (another live block there:
    /// `false`), the adjustment on it (already kept: `false`), the index
    /// continuing from `S + 1`. Its pending sends stay: each one's
    /// transaction index settles itself.
    fn settle<'a>(
        &'a self,
        index: &'a WalletIndex,
        holding: &'a Holding,
        header: &'a BlockHeader,
    ) -> Future<'a, anyhow::Result<bool>> {
        let (index, holding, header) = (index.clone(), holding.clone(), header.clone());
        Box::pin(self.db.write(move |connection| {
            let id = WalletIndexId::of(&index.address, &index.asset)?;
            let Some(block) = blocks::keep_bare(connection, index.network(), &header)? else {
                return Ok(false);
            };
            let mut totals = running(connection, &id, block.position)?;
            let amount = &holding.amount - &totals.total;
            let mut adjustment = Leg::adjustment(&block, &id, 0, &amount);
            adjustment.follow(&mut totals)?;
            if leg::db::is_kept(connection, &adjustment.id)? {
                return Ok(false);
            }
            leg::db::insert(connection, vec![&adjustment])?;
            wallet_index::db::settle(connection, &id, &block)?;
            Ok(true)
        }))
    }

    /// One write: the coins the index still shows unspent archived and
    /// written off ([`write_off`]), the index restarted, its pending sends
    /// forgotten (`jump`).
    fn restart<'a>(
        &'a self,
        index: &'a WalletIndex,
        birthday: u64,
    ) -> Future<'a, anyhow::Result<()>> {
        let index = index.clone();
        Box::pin(self.db.write(move |connection| {
            let id = WalletIndexId::of(&index.address, &index.asset)?;
            if let Some(at) = &index.progress.indexed_at {
                write_off(connection, &id, at, birthday)?;
            }
            wallet_index::db::restart(connection, &id, birthday)?;
            let sent = transaction::db::find_sent(connection, &id)?;
            transaction_indexes::forget(connection, &sent, Why::Jump)?;
            Ok(())
        }))
    }
}

/// Index `id`'s highest seq ever kept and totals at `position`.
fn running(
    connection: &mut SqliteConnection,
    id: &WalletIndexId,
    position: i64,
) -> anyhow::Result<leg::Running> {
    let ids = BTreeSet::from([String::from(id.clone())]);
    leg::db::find_running(connection, &ids, position)?
        .remove(id.as_ref())
        .context("the index's totals were not read")
}

/// Archives at `birthday`, the jump line, the coins index `id` still shows
/// unspent, and writes them off on `at`, the block it is indexed through:
/// one adjustment (leg 1) of −Σ them, which lowers its total only. None:
/// nothing.
fn write_off(
    connection: &mut SqliteConnection,
    id: &WalletIndexId,
    at: &BlockHeader,
    birthday: u64,
) -> anyhow::Result<()> {
    let Some(archived) = leg::db::archive(connection, id, int(birthday)?)? else {
        return Ok(());
    };
    let position = int(at.position)?;
    let block = block::db::find_by_position(connection, id.network(), position)?
        .context("the index's block is not kept")?;
    let mut adjustment = Leg::adjustment(&block, id, 1, &(Decimal::zero() - archived));
    adjustment.follow(&mut running(connection, id, position)?)?;
    leg::db::insert(connection, vec![&adjustment])?;
    Ok(())
}
