//! `wallet_index`: what the index observes — one wallet's address in one
//! asset, from its birthday — with how far it is inspected and its persisted
//! total. The row carries its own network and address, so nothing here reads
//! the `wallet` table; every read joins the cursor's block.

use num_bigint::BigInt;
use sqlx::{FromRow, SqliteConnection, sqlite::SqliteRow};

use super::{Block, column, failure, fetched, network, restored, store};
use crate::storage::watch::redefinition;
use crate::{
    Address, AssetId, BlockPosition, BlockRef, IndexError, IndexErrorKind, Network, Watch,
    WatchProgress,
};

/// The columns every read selects: the observation and its cursor's
/// `block_*` columns.
macro_rules! joined {
    ($tail:literal) => {
        concat!(
            "SELECT i.wallet_id, i.asset, i.birthday, i.balance, i.chain, i.network,
                    i.address, b.position AS block_position, b.height AS block_height,
                    b.hash AS block_hash, b.parents AS block_parents,
                    b.timestamp AS block_timestamp
             FROM wallet_index i LEFT JOIN block b ON b.id = i.block_id ",
            $tail
        )
    };
}

/// One `wallet_index` row.
pub(in crate::storage::sqlite) struct WalletIndex {
    wallet_id: String,
    network: Network,
    address: String,
    asset: String,
    birthday: i64,
    indexed: Option<Block>,
    balance: String,
}

impl FromRow<'_, SqliteRow> for WalletIndex {
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            wallet_id: column(row, "wallet_id")?,
            network: network(row)?,
            address: column(row, "address")?,
            asset: column(row, "asset")?,
            birthday: column(row, "birthday")?,
            indexed: Block::optional(row)?,
            balance: column(row, "balance")?,
        })
    }
}

impl WalletIndex {
    /// Every observation of `network`: the scan list.
    pub(in crate::storage::sqlite) async fn find_active(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> Result<Vec<Self>, IndexError> {
        let query = sqlx::query(joined!(
            "WHERE i.chain = ?1 AND i.network = ?2 ORDER BY i.wallet_id, i.asset"
        ))
        .bind(network.chain.as_str())
        .bind(network.name.as_str());
        fetched(connection, query, "failed to read the watch list").await
    }

    /// Every observation of `address`, whatever its asset,
    /// read inside the caller's own snapshot.
    ///
    /// What a query of the address's rows is honest with
    /// ([`crate::Inspection`]): the rows and the observations they were
    /// written for come from one transaction.
    pub(in crate::storage::sqlite) async fn find_active_by_address(
        connection: &mut SqliteConnection,
        network: &Network,
        address: &str,
    ) -> Result<Vec<Self>, IndexError> {
        let query = sqlx::query(joined!(
            "WHERE i.chain = ?1 AND i.network = ?2 AND i.address = ?3 ORDER BY i.asset"
        ))
        .bind(network.chain.as_str())
        .bind(network.name.as_str())
        .bind(address);
        fetched(connection, query, "failed to read the watch list").await
    }

    /// The observation of wallet `wallet_id` in `asset`, in any network;
    /// `RowNotFound` when none.
    pub(in crate::storage::sqlite) async fn get(
        connection: &mut SqliteConnection,
        wallet_id: &str,
        asset: &str,
    ) -> Result<Self, sqlx::Error> {
        sqlx::query_as(joined!("WHERE i.wallet_id = ?1 AND i.asset = ?2"))
            .bind(wallet_id)
            .bind(asset)
            .fetch_one(connection)
            .await
    }

    /// The observation of wallet `wallet_id` of `network` in `asset`;
    /// `RowNotFound` when none.
    pub(in crate::storage::sqlite) async fn get_in_network(
        connection: &mut SqliteConnection,
        network: &Network,
        wallet_id: &str,
        asset: &str,
    ) -> Result<Self, sqlx::Error> {
        sqlx::query_as(joined!(
            "WHERE i.chain = ?1 AND i.network = ?2 AND i.wallet_id = ?3 AND i.asset = ?4"
        ))
        .bind(network.chain.as_str())
        .bind(&network.name)
        .bind(wallet_id)
        .bind(asset)
        .fetch_one(connection)
        .await
    }

    /// The observation of `address` in `asset` in `network`;
    /// `RowNotFound` when none.
    pub(in crate::storage::sqlite) async fn get_by_holding(
        connection: &mut SqliteConnection,
        network: &Network,
        (address, asset): (&str, &str),
    ) -> Result<Self, sqlx::Error> {
        sqlx::query_as(joined!(
            "WHERE i.chain = ?1 AND i.network = ?2 AND i.address = ?3 AND i.asset = ?4"
        ))
        .bind(network.chain.as_str())
        .bind(&network.name)
        .bind(address)
        .bind(asset)
        .fetch_one(connection)
        .await
    }

    /// Adds `watch`'s observation with nothing inspected and `balance` as its
    /// total.
    pub(in crate::storage::sqlite) async fn insert(
        connection: &mut SqliteConnection,
        watch: &Watch,
        balance: &BigInt,
    ) -> Result<(), IndexError> {
        let birthday = i64::try_from(watch.start.0).map_err(|_| {
            // No chain has such a block, so the only way to ask for one is to
            // ask wrongly: the caller's mistake, which it can fix.
            IndexError::new(
                IndexErrorKind::InvalidRequest,
                "a watched start position is above the storable range",
                false,
            )
        })?;
        sqlx::query(
            "INSERT INTO wallet_index
                 (wallet_id, chain, network, address, asset, birthday, balance)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(&watch.id)
        .bind(watch.address.network.chain.as_str())
        .bind(&watch.address.network.name)
        .bind(&watch.address.value)
        .bind(&watch.asset.asset)
        .bind(birthday)
        .bind(balance.to_string())
        .execute(connection)
        .await
        .map(drop)
        .map_err(|error| Self::refused(&error))
    }

    /// A refused insert, by what refused it: the `(address, asset)` pair held
    /// under another id is a taken name, a wallet the database does not hold
    /// is the caller's missing step; both are not retryable conflicts.
    fn refused(error: &sqlx::Error) -> IndexError {
        let kind = error
            .as_database_error()
            .map(sqlx::error::DatabaseError::kind);
        match kind {
            Some(sqlx::error::ErrorKind::UniqueViolation) => redefinition(),
            Some(sqlx::error::ErrorKind::ForeignKeyViolation) => IndexError::new(
                IndexErrorKind::Conflict,
                "no wallet is filed under this watch's id",
                false,
            ),
            _ => failure(error, "failed to record a watch"),
        }
    }

    /// Removes the observation of wallet `wallet_id` of `network` in `asset`,
    /// if any.
    pub(in crate::storage::sqlite) async fn delete(
        connection: &mut SqliteConnection,
        network: &Network,
        wallet_id: &str,
        asset: &str,
    ) -> Result<(), IndexError> {
        sqlx::query(
            "DELETE FROM wallet_index
             WHERE chain = ?1 AND network = ?2 AND wallet_id = ?3 AND asset = ?4",
        )
        .bind(network.chain.as_str())
        .bind(&network.name)
        .bind(wallet_id)
        .bind(asset)
        .execute(connection)
        .await
        .map(drop)
        .map_err(|error| failure(&error, "failed to remove a watch"))
    }

    /// Records that this observation is inspected through `through`, a stored
    /// block of its network.
    pub(in crate::storage::sqlite) async fn update_progress(
        &self,
        connection: &mut SqliteConnection,
        through: &Block,
    ) -> Result<(), IndexError> {
        sqlx::query("UPDATE wallet_index SET block_id = ?3 WHERE wallet_id = ?1 AND asset = ?2")
            .bind(&self.wallet_id)
            .bind(&self.asset)
            .bind(through.id(&self.network))
            .execute(connection)
            .await
            .map(drop)
            .map_err(|error| failure(&error, "failed to record indexed progress"))
    }

    /// Adds `delta` to this observation's persisted total.
    pub(in crate::storage::sqlite) async fn update_balance(
        &self,
        connection: &mut SqliteConnection,
        delta: &BigInt,
    ) -> Result<(), IndexError> {
        let total = self.balance()? + delta;
        sqlx::query("UPDATE wallet_index SET balance = ?3 WHERE wallet_id = ?1 AND asset = ?2")
            .bind(&self.wallet_id)
            .bind(&self.asset)
            .bind(total.to_string())
            .execute(connection)
            .await
            .map(drop)
            .map_err(|error| failure(&error, "failed to write a watched balance"))
    }

    /// Resets the total of every observation of `network`, whose rows a purge
    /// is removing; the block delete already reset their progress.
    pub(in crate::storage::sqlite) async fn purge(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> Result<(), IndexError> {
        sqlx::query(
            "UPDATE wallet_index SET balance = '0', block_id = NULL
             WHERE chain = ?1 AND network = ?2",
        )
        .bind(network.chain.as_str())
        .bind(&network.name)
        .execute(connection)
        .await
        .map(drop)
        .map_err(|error| failure(&error, "failed to reset a purged network's claims"))
    }

    /// The asset this row observes.
    pub(in crate::storage::sqlite) fn asset(&self) -> &str {
        &self.asset
    }

    /// How far this row is inspected; `None` when nothing has been recorded.
    pub(in crate::storage::sqlite) fn progress(&self) -> Result<Option<BlockRef>, IndexError> {
        self.indexed.clone().map(BlockRef::try_from).transpose()
    }

    /// The persisted total.
    pub(in crate::storage::sqlite) fn balance(&self) -> Result<BigInt, IndexError> {
        self.balance
            .parse()
            .map_err(|_| store("a stored balance is not an atomic integer"))
    }
}

impl TryFrom<WalletIndex> for WatchProgress {
    type Error = IndexError;

    /// The observation a row holds, with the progress recorded for it.
    fn try_from(row: WalletIndex) -> Result<Self, IndexError> {
        let indexed_through = row.progress()?;
        Ok(Self {
            watch: Watch {
                id: row.wallet_id,
                asset: AssetId {
                    chain: row.network.chain.clone(),
                    asset: row.asset,
                },
                address: Address {
                    network: row.network,
                    value: row.address,
                },
                start: BlockPosition(restored(row.birthday)?),
            },
            indexed_through,
        })
    }
}
