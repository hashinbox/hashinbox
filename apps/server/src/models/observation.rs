//! What indexing observes at one address: its `wallet` rows that are not
//! archived, with their birthday, progress and kept total.

use base::{Address, AssetId, BlockHash, BlockHeight, BlockPosition, BlockRef};
use indexing::{IndexError, IndexErrorKind, Watch, WatchProgress};
use num_bigint::BigInt;
use sqlx::{Row as _, SqliteConnection, sqlite::SqliteRow};

/// Every observation of one address, whatever its asset: what a read of the
/// address's rows says about how far they have been inspected.
pub(crate) struct Observations(Vec<Observation>);

impl Observations {
    /// The signed total indexing keeps for `asset`'s observation, if observed.
    pub(crate) fn kept(&self, asset: &AssetId) -> Option<BigInt> {
        self.0
            .iter()
            .find(|observed| &observed.progress.watch.asset == asset)
            .and_then(|observed| observed.balance.clone())
    }

    /// How many blocks the address's inspection is behind `checkpoint`: zero
    /// when every observation has caught up to it (or there is no line yet),
    /// otherwise the produced-height gap of the furthest-behind observation,
    /// at least one.
    ///
    /// Address-scoped, not per observation: one inspection of an address
    /// serves every asset observed at it, so rows of one asset are still
    /// arriving while a sibling's catch-up is. An observation with no progress
    /// yet is measured from its birthday, read as a height; on chains whose
    /// positions are not heights (Solana slots) that is a lower bound.
    pub(crate) fn behind(&self, checkpoint: Option<&BlockRef>) -> u64 {
        let Some(line) = checkpoint else {
            return 0;
        };
        self.0
            .iter()
            .filter(|observed| !observed.progress.caught_up(Some(line)))
            .map(|observed| {
                let from = observed.progress.indexed_through.as_ref().map_or_else(
                    || observed.progress.watch.start.0.saturating_sub(1),
                    |through| through.height.0,
                );
                line.height.0.saturating_sub(from).max(1)
            })
            .max()
            .unwrap_or(0)
    }
}

/// One observed row: the watch, how far it is inspected, and its signed total.
pub(crate) struct Observation {
    progress: WatchProgress,
    /// The holding's signed net in atomic units, kept by every commit.
    balance: Option<BigInt>,
}

impl Observation {
    /// Every observed row of `address`, whatever its asset.
    ///
    /// # Errors
    ///
    /// A retryable `Store` error when the read fails, a terminal one for a
    /// malformed row.
    pub(crate) async fn find_by_address(
        connection: &mut SqliteConnection,
        address: &Address,
    ) -> Result<Observations, IndexError> {
        let network = &address.network;
        let rows = sqlx::query(
            "SELECT id, asset, birthday, block_position, block_height, block_hash, balance
             FROM wallet
             WHERE chain = ?1 AND network = ?2 AND address = ?3 AND archived_at IS NULL",
        )
        .bind(network.chain.as_str())
        .bind(&network.name)
        .bind(&address.value)
        .fetch_all(connection)
        .await
        .map_err(|_| IndexError::new(IndexErrorKind::Store, "failed to read wallets", true))?;
        rows.iter()
            .map(|row| Self::from_row(row, address))
            .collect::<Result<Vec<_>, _>>()
            .map(Observations)
    }

    /// One `wallet` row of `address` as the observation it holds.
    fn from_row(row: &SqliteRow, address: &Address) -> Result<Self, IndexError> {
        let malformed =
            || IndexError::new(IndexErrorKind::Store, "a stored wallet is malformed", false);
        let unsigned = |value: i64| u64::try_from(value).map_err(|_| malformed());
        let id: String = row.try_get("id").map_err(|_| malformed())?;
        let asset: String = row.try_get("asset").map_err(|_| malformed())?;
        let birthday: i64 = row.try_get("birthday").map_err(|_| malformed())?;
        let position: Option<i64> = row.try_get("block_position").map_err(|_| malformed())?;
        let height: Option<i64> = row.try_get("block_height").map_err(|_| malformed())?;
        let hash: Option<Vec<u8>> = row.try_get("block_hash").map_err(|_| malformed())?;
        let balance: Option<String> = row.try_get("balance").map_err(|_| malformed())?;
        let indexed_through = match (position, height, hash) {
            (Some(position), Some(height), Some(hash)) => Some(BlockRef {
                position: BlockPosition(unsigned(position)?),
                height: BlockHeight(unsigned(height)?),
                hash: BlockHash(hash),
                parent: None,
                timestamp: None,
            }),
            (None, None, None) => None,
            _ => return Err(malformed()),
        };
        let watch = Watch {
            id,
            address: address.clone(),
            asset: AssetId {
                chain: address.network.chain.clone(),
                asset,
            },
            start: BlockPosition(unsigned(birthday)?),
        };
        let balance = balance
            .map(|text| text.parse::<BigInt>().map_err(|_| malformed()))
            .transpose()?;
        Ok(Self {
            progress: WatchProgress {
                watch,
                indexed_through,
            },
            balance,
        })
    }
}

#[cfg(test)]
mod tests {
    use base::{ChainName, Network};

    use super::*;

    fn block(height: u64) -> BlockRef {
        BlockRef {
            position: BlockPosition(height),
            height: BlockHeight(height),
            hash: BlockHash(vec![height as u8]),
            parent: None,
            timestamp: None,
        }
    }

    fn observed(start: u64, through: Option<u64>) -> Observation {
        let network = Network::new(ChainName::new("test"), "net");
        Observation {
            progress: WatchProgress {
                watch: Watch {
                    id: format!("w-{start}"),
                    address: Address::new(network.clone(), "a"),
                    asset: AssetId::native(network.chain),
                    start: BlockPosition(start),
                },
                indexed_through: through.map(block),
            },
            balance: None,
        }
    }

    #[test]
    fn behind_is_the_furthest_observation_gap_and_zero_once_caught_up() {
        let line = block(10);
        let caught_up = Observations(vec![observed(0, Some(10)), observed(11, None)]);
        assert_eq!(caught_up.behind(Some(&line)), 0);
        let catching_up = Observations(vec![observed(0, Some(10)), observed(3, Some(6))]);
        assert_eq!(catching_up.behind(Some(&line)), 4);
        let unread = Observations(vec![observed(5, None)]);
        assert_eq!(unread.behind(Some(&line)), 6, "blocks 5 through 10");
        assert_eq!(unread.behind(None), 0, "no line to be behind");
    }
}
