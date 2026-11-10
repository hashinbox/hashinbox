//! `leg`: the kept legs of transactions (no block while pending) and the kept rewards of
//! blocks; a `Received` leg with a coin is a UTXO, spent while a `Spent` leg
//! in a live block names it. A mined leg is its wallet index's and carries
//! the index's running balance (`total`, design 072) and what it has
//! received and sent so far, in (`position`, `seq`) order.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context as _;
use base::{Address, Addressed as _, Asset, Coin, Decimal, Input, Network, Reward, TransactionId};
use diesel::{
    prelude::*,
    sql_types::{BigInt, Integer, Nullable, Text},
    sqlite::Sqlite,
};

use super::{
    Json, child, int,
    schema::{leg, wallet_index_with_balance},
    uint,
    wallet_index::WalletIndex,
};

/// The `kind` of a reward row.
const REWARD: i32 = 5;

/// One `leg` row: a transaction's leg, or a block's reward; the API shows
/// it as is.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    Queryable,
    Selectable,
    Insertable,
    QueryableByName,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[diesel(table_name = leg, check_for_backend(Sqlite))]
pub(crate) struct Leg {
    pub(crate) id: String,
    /// Its block; null while pending.
    pub(crate) block: Option<String>,
    /// Its transaction; null for a reward.
    pub(crate) transaction: Option<String>,
    pub(crate) leg_index: i64,
    /// 0 received, 1 spent, 2 burned, 3 minted, 4 fee, 5 reward.
    pub(crate) kind: i32,
    pub(crate) address: String,
    pub(crate) asset: String,
    /// Exact atomic decimal text.
    pub(crate) amount: String,
    pub(crate) coin_txid: Option<String>,
    pub(crate) coin_index: Option<i64>,
    /// The wallet index it is kept for: its address's index in its asset.
    pub(crate) wallet_index_id: String,
    /// Its block's position; `None` while pending.
    pub(crate) position: Option<i64>,
    /// Its index's leg sequence: 1 + the highest the index has kept,
    /// unique per index; `None` while pending.
    pub(crate) seq: Option<i64>,
    /// Its index's balance right after it; `None` while pending.
    pub(crate) total: Option<String>,
    /// What its index has received (received, minted, reward) through it;
    /// `None` while pending.
    pub(crate) total_received: Option<String>,
    /// What its index has sent (spent, burned, fee) through it; `None`
    /// while pending. `total = total_received - total_sent`.
    pub(crate) total_sent: Option<String>,
}

/// Legs parsed from a JSON column (an array SQLite built).
impl From<Json<Vec<Leg>>> for Vec<Leg> {
    fn from(Json(legs): Json<Vec<Leg>>) -> Self {
        legs
    }
}

/// Leg `index` of transaction `transaction` (its id), pending: id
/// `"{transaction}.{index}"`, no block, position, seq or total ([`Block`]
/// mines it, [`block::db::save`] numbers it).
///
/// [`Block`]: super::Block
/// [`block::db::save`]: super::block::db::save
impl TryFrom<(&str, u32, &base::Leg)> for Leg {
    type Error = anyhow::Error;

    fn try_from((transaction, index, leg): (&str, u32, &base::Leg)) -> anyhow::Result<Self> {
        let (kind, coin) = match leg {
            base::Leg::Received { coin, .. } => (0, coin.as_ref()),
            base::Leg::Spent { coin, .. } => (1, coin.as_ref()),
            base::Leg::Burned { .. } => (2, None),
            base::Leg::Minted { .. } => (3, None),
            base::Leg::Fee { .. } => (4, None),
        };
        Ok(Self {
            id: child(transaction, &[&index.to_string()])?,
            block: None,
            transaction: Some(transaction.to_owned()),
            leg_index: i64::from(index),
            kind,
            address: leg.address().value.clone(),
            asset: leg.asset().id.clone(),
            amount: leg.amount().to_string(),
            coin_txid: coin.map(|coin| coin.transaction.id.clone()),
            coin_index: coin.map(|coin| i64::from(coin.index)),
            wallet_index_id: WalletIndex::id(leg.address(), leg.asset())?,
            position: None,
            seq: None,
            total: None,
            total_received: None,
            total_sent: None,
        })
    }
}

/// `reward` of block `block` (its id): id `"{block}.reward.{index}"`, no
/// position, seq or total yet.
impl TryFrom<(&str, &Reward)> for Leg {
    type Error = anyhow::Error;

    fn try_from((block, reward): (&str, &Reward)) -> anyhow::Result<Self> {
        let index = int(reward.index)?;
        Ok(Self {
            id: child(block, &["reward", &index.to_string()])?,
            block: Some(block.to_owned()),
            transaction: None,
            leg_index: index,
            kind: REWARD,
            address: reward.address.value.clone(),
            asset: reward.asset.id.clone(),
            amount: reward.amount.to_string(),
            coin_txid: None,
            coin_index: None,
            wallet_index_id: WalletIndex::id(&reward.address, &reward.asset)?,
            position: None,
            seq: None,
            total: None,
            total_received: None,
            total_sent: None,
        })
    }
}

impl Leg {
    /// What it adds to its address's balance: [`base::Leg::signed`]'s rule,
    /// and a reward's own sign.
    ///
    /// # Errors
    ///
    /// When the stored amount or kind does not decode.
    pub(crate) fn signed(&self) -> anyhow::Result<Decimal> {
        let amount: Decimal = self
            .amount
            .parse()
            .context("a stored amount does not decode")?;
        Ok(match self.kind {
            0 | 3 | REWARD => amount,
            1 | 2 | 4 => Decimal::zero() - amount,
            _ => anyhow::bail!("a stored leg kind is unknown"),
        })
    }
}

impl Leg {
    /// The transaction leg this row was written from, of `network` (the row
    /// holds none).
    ///
    /// # Errors
    ///
    /// When a stored value does not decode, or it is a reward.
    pub(crate) fn leg(self, network: &Network) -> anyhow::Result<base::Leg> {
        let address = Address::new(network.clone(), self.address);
        let asset = Asset {
            network: network.clone(),
            id: self.asset,
        };
        let amount: Decimal = self
            .amount
            .parse()
            .context("a stored amount does not decode")?;
        let coin = match (self.coin_txid, self.coin_index) {
            (Some(txid), Some(index)) => Some(Coin {
                transaction: TransactionId::new(network.clone(), txid),
                index: u32::try_from(uint(index)?)
                    .context("a stored coin index is out of range")?,
            }),
            _ => None,
        };
        Ok(match self.kind {
            0 => base::Leg::Received {
                address,
                asset,
                amount,
                coin,
            },
            1 => base::Leg::Spent {
                address,
                asset,
                amount,
                coin,
            },
            2 => base::Leg::Burned {
                address,
                asset,
                amount,
            },
            3 => base::Leg::Minted {
                address,
                asset,
                amount,
            },
            4 => base::Leg::Fee {
                address,
                asset,
                amount,
            },
            _ => anyhow::bail!("a stored leg kind is not a transaction leg"),
        })
    }
}

/// Its `seq`.
impl crate::db::Keyed for Leg {
    fn key(&self) -> String {
        self.seq.unwrap_or_default().to_string()
    }
}

/// A mined leg with its transaction's txid and state (null for a reward):
/// a row of [`db::find_page`].
#[derive(QueryableByName, serde::Serialize, utoipa::ToSchema)]
pub(crate) struct Movement {
    #[diesel(embed)]
    #[serde(flatten)]
    pub(crate) leg: Leg,
    #[diesel(sql_type = Nullable<Text>)]
    pub(crate) txid: Option<String>,
    /// 0 pending, 1 succeeded, 2 reverted.
    #[diesel(sql_type = Nullable<Integer>)]
    pub(crate) state: Option<i32>,
}

/// Its leg's `seq`.
impl crate::db::Keyed for Movement {
    fn key(&self) -> String {
        self.leg.key()
    }
}

/// The `leg` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;
    use crate::db::Page;

    /// A coin with its creating block's height.
    #[derive(QueryableByName)]
    struct Unspent {
        #[diesel(embed)]
        leg: Leg,
        #[diesel(sql_type = BigInt)]
        height: i64,
    }

    /// A row of [`find_running`]'s query.
    #[derive(QueryableByName)]
    struct Running {
        #[diesel(sql_type = Text)]
        id: String,
        #[diesel(sql_type = BigInt)]
        seq: i64,
        #[diesel(sql_type = Text)]
        total_received: String,
        #[diesel(sql_type = Text)]
        total_sent: String,
    }

    /// A kept wallet index a pending transaction moves funds of: a row of
    /// [`find_spending`]'s query.
    #[derive(Queryable, Selectable)]
    #[diesel(table_name = wallet_index_with_balance)]
    pub(crate) struct Spending {
        pub(crate) id: String,
        balance_confirmed_spendable: Option<String>,
    }

    impl Spending {
        /// Whether it can spend `takes` (signed: what a send takes is
        /// negative): its `balance_confirmed_spendable` (zero while unknown) plus
        /// `takes` stays at or above zero.
        ///
        /// # Errors
        ///
        /// When a stored amount does not decode.
        pub(crate) fn can_spend(&self, takes: Decimal) -> anyhow::Result<bool> {
            let spendable: Decimal = self
                .balance_confirmed_spendable
                .as_deref()
                .unwrap_or("0")
                .parse()
                .context("a stored balance does not decode")?;
            Ok(spendable + takes >= Decimal::zero())
        }
    }

    /// At most `page.limit` live mined legs of wallet index `index` with a
    /// seq below `page.after` when given, latest first, each with its
    /// transaction's txid and state.
    pub(crate) fn find_page(
        connection: &mut SqliteConnection,
        index: &str,
        page: Page<i64>,
    ) -> QueryResult<Vec<Movement>> {
        // At most `?3` live mined legs of wallet index `?1` with a seq below
        // `?2`, latest first (an index's seq grows with its live legs'
        // positions); a reward has no transaction.
        diesel::sql_query(
            r#"
            SELECT
                l.*,
                t.txid,
                t.state
            FROM leg l
            JOIN block b ON b.id = l.block
            LEFT JOIN "transaction" t ON t.id = l."transaction"
            WHERE l.wallet_index_id = ?1
              AND l.seq < ?2
              AND b.archived_at IS NULL
            ORDER BY l.seq DESC
            LIMIT ?3"#,
        )
        .bind::<Text, _>(index)
        .bind::<BigInt, _>(page.after.unwrap_or(i64::MAX))
        .bind::<BigInt, _>(page.limit)
        .load(connection)
    }

    /// Where each of wallet indexes `indexes` continues from at
    /// `position`: its highest seq ever kept and what it had received and
    /// sent there, by id.
    ///
    /// # Errors
    ///
    /// When the read fails or a stored total does not decode.
    pub(crate) fn find_running(
        connection: &mut SqliteConnection,
        indexes: &BTreeSet<String>,
        position: i64,
    ) -> anyhow::Result<BTreeMap<String, (i64, Decimal, Decimal)>> {
        // Where each wallet index of `?1` (a JSON list of ids) continues from
        // at position `?2`: the highest seq it has ever kept (0: none) and the
        // received and sent totals of its last live mined leg at or below
        // `?2` (`last`; '0': none).
        let rows: Vec<Running> = diesel::sql_query(
            "
            SELECT
                i.value AS id,
                (
                    SELECT COALESCE(MAX(seq), 0)
                    FROM leg
                    WHERE wallet_index_id = i.value
                ) AS seq,
                COALESCE(last.total_received, '0') AS total_received,
                COALESCE(last.total_sent, '0') AS total_sent
            FROM json_each(?1) i
            LEFT JOIN leg last ON last.id = (
                SELECT l.id
                FROM leg l
                JOIN block b ON b.id = l.block
                WHERE l.wallet_index_id = i.value
                  AND l.position <= ?2
                  AND b.archived_at IS NULL
                ORDER BY l.position DESC, l.seq DESC
                LIMIT 1
            )",
        )
        .bind::<Text, _>(serde_json::to_string(indexes)?)
        .bind::<BigInt, _>(position)
        .load(connection)?;
        rows.into_iter()
            .map(|row| {
                let received = row
                    .total_received
                    .parse()
                    .context("a stored total does not decode")?;
                let sent = row
                    .total_sent
                    .parse()
                    .context("a stored total does not decode")?;
                Ok((row.id, (row.seq, received, sent)))
            })
            .collect()
    }

    /// Deletes the legs of block `block` kept for wallet indexes `indexes`
    /// (a block kept again writes them again).
    pub(crate) fn delete_in(
        connection: &mut SqliteConnection,
        block: &str,
        indexes: &BTreeSet<String>,
    ) -> QueryResult<usize> {
        diesel::delete(
            leg::table
                .filter(leg::block.eq(block))
                .filter(leg::wallet_index_id.eq_any(indexes)),
        )
        .execute(connection)
    }

    /// The kept wallet indexes of `indexes` as a pending transaction spends
    /// from them.
    pub(crate) fn find_spending(
        connection: &mut SqliteConnection,
        indexes: Vec<&String>,
    ) -> QueryResult<Vec<Spending>> {
        wallet_index_with_balance::table
            .filter(wallet_index_with_balance::id.eq_any(indexes))
            .select(Spending::as_select())
            .load(connection)
    }

    /// Keeps `rows`, in one statement.
    pub(crate) fn insert(connection: &mut SqliteConnection, rows: Vec<&Leg>) -> QueryResult<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        diesel::insert_into(leg::table)
            .values(rows)
            .execute(connection)
    }

    /// Deletes the legs of transaction `transaction` of any block but
    /// `block`, or none (pending): it is mined in `block` now.
    pub(crate) fn delete_moved(
        connection: &mut SqliteConnection,
        block: &str,
        transaction: &str,
    ) -> QueryResult<usize> {
        diesel::delete(
            leg::table
                .filter(leg::transaction.eq(transaction))
                .filter(leg::block.is_null().or(leg::block.ne(block))),
        )
        .execute(connection)
    }

    /// How many legs (movements and rewards) every network keeps.
    pub(crate) fn count(connection: &mut SqliteConnection) -> QueryResult<i64> {
        leg::table.count().get_result(connection)
    }

    /// Every unspent coin of `addresses` (all of `network`), as inputs, in
    /// chain order.
    ///
    /// # Errors
    ///
    /// When the read fails or a stored value does not decode.
    pub(crate) fn find_unspent(
        connection: &mut SqliteConnection,
        network: &Network,
        addresses: &[&str],
    ) -> anyhow::Result<Vec<Input>> {
        // The `Received` legs of an address with a coin, in live blocks, that no
        // `Spent` leg of the same network consumes, in a live block or in a
        // pending transaction (spent from broadcast on).
        let rows: Vec<Unspent> = diesel::sql_query(
            r#"
            SELECT
                l.*,
                b.height AS height
            FROM leg l
            JOIN block b ON b.id = l.block
            JOIN "transaction" t ON t.id = l."transaction"
            WHERE b.chain = ?1
              AND b.network = ?2
              AND l.address IN (SELECT value FROM json_each(?3))
              AND l.kind = 0
              AND l.coin_txid IS NOT NULL
              AND b.archived_at IS NULL
              AND NOT EXISTS (
                  SELECT 1
                  FROM leg s
                  JOIN block sb ON sb.id = s.block
                  WHERE s.kind = 1
                    AND s.coin_txid = l.coin_txid
                    AND s.coin_index = l.coin_index
                    AND sb.chain = b.chain
                    AND sb.network = b.network
                    AND sb.archived_at IS NULL
              )
              AND NOT EXISTS (
                  SELECT 1
                  FROM leg s
                  JOIN "transaction" st ON st.id = s."transaction"
                  WHERE s.kind = 1
                    AND s.coin_txid = l.coin_txid
                    AND s.coin_index = l.coin_index
                    AND s.block IS NULL
                    AND st.block IS NULL
                    AND st.chain = b.chain
                    AND st.network = b.network
              )
            ORDER BY b.position, t."order", l.leg_index"#,
        )
        .bind::<Text, _>(network.chain.as_str())
        .bind::<Text, _>(&network.name)
        .bind::<Text, _>(serde_json::to_string(addresses)?)
        .load(connection)?;
        rows.into_iter()
            .map(|row| {
                let height = uint(row.height)?;
                match row.leg.leg(network)? {
                    base::Leg::Received {
                        address,
                        asset,
                        amount,
                        coin: Some(coin),
                    } => Ok(Input {
                        coin,
                        address,
                        asset,
                        amount,
                        height,
                    }),
                    _ => anyhow::bail!("a stored coin is not a received leg"),
                }
            })
            .collect()
    }
}
