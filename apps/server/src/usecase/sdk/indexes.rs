//! `WalletIndexStore` on the `wallet_index` model; an index's wallet is kept
//! with it (watch-only when its key is not kept). A settle and a restart
//! write on the `leg`, `transaction` and `transaction_index` models too.

use base::{Future, Holding, IndexState, IntoIndex, Network, WalletIndex, WalletIndexStore};

use super::Stores;
use crate::models::{
    Leg, Wallet, int,
    leg::{self, Legs},
    transaction,
    transaction_index::Why,
    uint, wallet, wallet_index,
    wallet_index::WalletIndexId,
};
use crate::usecase::indexes::{transaction as transactions, wallet as wallets};

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
    /// One write, no block kept: the adjustment at `S` (already kept:
    /// `false`; none when it would be zero), the index continuing from
    /// `S + 1`. Its pending sends stay: each one's transaction index settles
    /// itself. Once written, logged as `index.settle` (`wallet asset
    /// position stored_total chain_balance adjustment`): debug when nothing
    /// changed, warn with `why` when the balance was adjusted.
    fn settle<'a>(
        &'a self,
        index: &'a WalletIndex,
        holding: &'a Holding,
    ) -> Future<'a, anyhow::Result<bool>> {
        let (written, held) = (index.clone(), holding.clone());
        Box::pin(async move {
            let settled = self.db.write(move |connection| {
                let id = WalletIndexId::of(&written.address, &written.asset)?;
                let position = int(held.position)?;
                let stored = leg::db::find_total(connection, &id, position)?;
                let amount = &held.amount - &stored;
                if amount != base::Decimal::zero() {
                    let adjustment = Leg::adjustment(position, &id, 0, &amount)?;
                    if leg::db::is_kept(connection, &adjustment.id)? {
                        return Ok(None);
                    }
                    leg::db::append(connection, &mut Legs::from(vec![adjustment]), position)?;
                }
                wallet_index::db::settle(connection, &id, position)?;
                Ok(Some((stored, amount)))
            });
            let Some((stored_total, adjustment)) = settled.await? else {
                return Ok(false);
            };
            let (wallet, asset) = (index.address.value.as_str(), index.asset.id.as_str());
            let (position, chain_balance) = (holding.position, &holding.amount);
            if adjustment == base::Decimal::zero() {
                tracing::debug!(
                    target: "index.settle",
                    wallet, asset, position,
                    stored_total = %stored_total, chain_balance = %chain_balance, adjustment = %adjustment,
                    "Wallet index settled: no change"
                );
            } else {
                let why = "the chain's balance is not the kept legs' total: the history has a gap";
                tracing::warn!(
                    target: "index.settle",
                    wallet, asset, position,
                    stored_total = %stored_total, chain_balance = %chain_balance, adjustment = %adjustment, why,
                    "Wallet index settled: balance adjusted"
                );
            }
            Ok(true)
        })
    }

    /// One write: the coins the index still shows unspent archived and
    /// written off ([`wallets::write_off`]), the index restarted, its pending sends
    /// forgotten (`jump`).
    fn restart<'a>(
        &'a self,
        index: &'a WalletIndex,
        birthday: u64,
    ) -> Future<'a, anyhow::Result<()>> {
        let index = index.clone();
        Box::pin(self.db.write(move |connection| {
            let id = WalletIndexId::of(&index.address, &index.asset)?;
            if let Some(at) = index.progress.indexed_at {
                wallets::write_off(connection, &id, at, birthday)?;
            }
            wallet_index::db::restart(connection, &id, birthday)?;
            let sent = transaction::db::find_sent(connection, &id)?;
            transactions::forget(connection, &sent, Why::Jump)?;
            Ok(())
        }))
    }
}
