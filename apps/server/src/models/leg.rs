//! `leg`: the kept legs of transactions (no block while pending), the kept rewards of
//! blocks, and the adjustments of indexes (design 083); a `Received` leg with a coin is a UTXO, spent while a `Spent` leg
//! in a live block names it, never spendable once archived (a jump wrote it
//! off). A mined leg is its wallet index's and carries
//! the index's running balance (`total`, design 072) and what the chain
//! showed it receive and send so far, in (`position`, `seq`) order.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context as _;
use base::{Address, Addressed as _, Asset, Coin, Decimal, Input, Network, Networked, Reward};
use diesel::{
    prelude::*,
    sql_types::{BigInt, Integer, Nullable, Text},
    sqlite::Sqlite,
};

use super::{
    Id, IdError, Json,
    block::BlockId,
    int,
    schema::{leg, wallet_index_with_balance},
    transaction::TransactionId,
    uint,
    wallet_index::WalletIndexId,
};

/// A leg's id: under its transaction, `{transaction}.{index}`; a block
/// reward, `{block}.reward.{index}`.
pub(crate) enum LegId<'a> {
    /// Leg `index` of transaction id `.0`.
    Transaction(&'a str, u32),
    /// Reward `index` of block id `.0`.
    Reward(&'a str, i64),
}

impl LegId<'_> {
    /// The row id.
    ///
    /// # Errors
    ///
    /// When a part holds a dot.
    pub(crate) fn id(&self) -> Result<Id, IdError> {
        match self {
            Self::Transaction(transaction, index) => Id::child(transaction, &[&index.to_string()]),
            Self::Reward(block, index) => Id::child(block, &["reward", &index.to_string()]),
        }
    }
}

/// The `kind` of a reward row.
const REWARD: i32 = 5;
/// The `kind` of an adjustment row (design 083 §7).
const ADJUSTMENT: i32 = 6;

/// One `leg` row: a transaction's leg, a block's reward, or an index's
/// adjustment; the API shows it as is.
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
    #[schema(value_type = Option<String>)]
    pub(crate) block: Option<BlockId>,
    /// Its transaction; null for a reward or an adjustment.
    #[schema(value_type = Option<String>)]
    pub(crate) transaction: Option<TransactionId>,
    pub(crate) leg_index: i64,
    /// 0 received, 1 spent, 2 burned, 3 minted, 4 fee, 5 reward, 6
    /// adjustment (signed; leg 0: what the chain's balance read corrects the
    /// index's total by, at the block it was read at; leg 1: the coins a
    /// jump archived).
    pub(crate) kind: i32,
    pub(crate) address: String,
    pub(crate) asset: String,
    /// Exact atomic decimal text; signed for a reward or an adjustment.
    pub(crate) amount: String,
    pub(crate) coin_txid: Option<String>,
    pub(crate) coin_index: Option<i64>,
    /// The wallet index it is kept for: its address's index in its asset.
    #[schema(value_type = String)]
    pub(crate) wallet_index_id: WalletIndexId,
    /// Its block's position; `None` while pending.
    pub(crate) position: Option<i64>,
    /// Its index's leg sequence: 1 + the highest the index has kept,
    /// unique per index; `None` while pending.
    pub(crate) seq: Option<i64>,
    /// Its index's balance right after it; `None` while pending.
    pub(crate) total: Option<String>,
    /// What its index has received (received, minted, reward) through it;
    /// `None` while pending. No adjustment counts.
    pub(crate) total_received: Option<String>,
    /// What its index has sent (spent, burned, fee) through it; `None` while
    /// pending. `total` is `total_received - total_sent` plus its index's
    /// adjustments so far.
    pub(crate) total_sent: Option<String>,
    /// A received coin a jump wrote off: that jump's position; never
    /// spendable again. Null otherwise.
    pub(crate) archived_at: Option<i64>,
}

/// Legs parsed from a JSON column (an array SQLite built).
impl From<Json<Vec<Leg>>> for Vec<Leg> {
    fn from(Json(legs): Json<Vec<Leg>>) -> Self {
        legs
    }
}

/// Leg `index` of transaction `transaction`, pending: id
/// `"{transaction}.{index}"`, no block, position, seq or total ([`Block`]
/// mines it, [`block::db::save`] numbers it).
///
/// [`Block`]: super::Block
/// [`block::db::save`]: super::block::db::save
impl TryFrom<(&TransactionId, u32, &base::Leg)> for Leg {
    type Error = anyhow::Error;

    fn try_from(
        (transaction, index, leg): (&TransactionId, u32, &base::Leg),
    ) -> anyhow::Result<Self> {
        let (kind, coin) = match leg {
            base::Leg::Received { coin, .. } => (0, coin.as_ref()),
            base::Leg::Spent { coin, .. } => (1, coin.as_ref()),
            base::Leg::Burned { .. } => (2, None),
            base::Leg::Minted { .. } => (3, None),
            base::Leg::Fee { .. } => (4, None),
        };
        Ok(Self {
            id: LegId::Transaction(transaction.as_ref(), index).id()?.into(),
            block: None,
            transaction: Some(transaction.clone()),
            leg_index: i64::from(index),
            kind,
            address: leg.address().value.clone(),
            asset: leg.asset().id.clone(),
            amount: leg.amount().to_string(),
            coin_txid: coin.map(|coin| coin.transaction.id.clone()),
            coin_index: coin.map(|coin| i64::from(coin.index)),
            wallet_index_id: WalletIndexId::of(leg.address(), leg.asset())?,
            position: None,
            seq: None,
            total: None,
            total_received: None,
            total_sent: None,
            archived_at: None,
        })
    }
}

/// `reward` of block `block`: id `"{block}.reward.{index}"`, no position,
/// seq or total yet.
impl TryFrom<(&BlockId, &Reward)> for Leg {
    type Error = anyhow::Error;

    fn try_from((block, reward): (&BlockId, &Reward)) -> anyhow::Result<Self> {
        let index = int(reward.index)?;
        Ok(Self {
            id: LegId::Reward(block.as_ref(), index).id()?.into(),
            block: Some(block.clone()),
            transaction: None,
            leg_index: index,
            kind: REWARD,
            address: reward.address.value.clone(),
            asset: reward.asset.id.clone(),
            amount: reward.amount.to_string(),
            coin_txid: None,
            coin_index: None,
            wallet_index_id: WalletIndexId::of(&reward.address, &reward.asset)?,
            position: None,
            seq: None,
            total: None,
            total_received: None,
            total_sent: None,
            archived_at: None,
        })
    }
}

impl Networked for Leg {
    fn network(&self) -> &Network {
        self.wallet_index_id.network()
    }
}

/// Where an index's legs continue from ([`db::find_running`]): its highest
/// seq ever kept, and its last live leg's total, received and sent.
pub(crate) struct Running {
    pub(crate) seq: i64,
    pub(crate) total: Decimal,
    pub(crate) received: Decimal,
    pub(crate) sent: Decimal,
}

impl Leg {
    /// Adjustment `leg_index` (0: a settle, 1: a write-off) of index
    /// `index` on `block`: `amount`, signed; numbered by [`Self::follow`].
    pub(crate) fn adjustment(
        block: &super::Block,
        index: &WalletIndexId,
        leg_index: i64,
        amount: &Decimal,
    ) -> Self {
        Self {
            id: format!(
                "{}.adjustment.{}.{leg_index}",
                block.id.as_ref(),
                index.as_ref()
            ),
            block: Some(block.id.clone()),
            transaction: None,
            leg_index,
            kind: ADJUSTMENT,
            address: index.address().value.clone(),
            asset: index.asset.clone(),
            amount: amount.to_string(),
            coin_txid: None,
            coin_index: None,
            wallet_index_id: index.clone(),
            position: Some(block.position),
            seq: None,
            total: None,
            total_received: None,
            total_sent: None,
            archived_at: None,
        }
    }

    /// Numbers it after `running`, its index's highest seq and totals so
    /// far ([`db::find_running`]), and moves them on to it: it takes the
    /// next seq and adds its signed amount to the total; a received,
    /// minted or reward leg counts as received, a spent, burned or fee leg
    /// as sent, an adjustment as neither.
    ///
    /// # Errors
    ///
    /// When its amount or kind does not decode.
    pub(crate) fn follow(&mut self, running: &mut Running) -> anyhow::Result<()> {
        let signed = self.signed()?;
        running.total += &signed;
        match self.kind {
            ADJUSTMENT => {}
            0 | 3 | REWARD => running.received += signed,
            _ => running.sent -= signed,
        }
        running.seq += 1;
        self.seq = Some(running.seq);
        self.total = Some(running.total.to_string());
        self.total_received = Some(running.received.to_string());
        self.total_sent = Some(running.sent.to_string());
        Ok(())
    }

    /// What it adds to its address's balance: [`base::Leg::signed`]'s rule,
    /// and a reward's or an adjustment's own sign.
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
            0 | 3 | REWARD | ADJUSTMENT => amount,
            1 | 2 | 4 => Decimal::zero() - amount,
            _ => anyhow::bail!("a stored leg kind is unknown"),
        })
    }
}

impl Leg {
    /// The transaction leg this row was written from.
    ///
    /// # Errors
    ///
    /// When a stored value does not decode, or it is a reward or an
    /// adjustment.
    pub(crate) fn leg(self) -> anyhow::Result<base::Leg> {
        let network = self.wallet_index_id.network();
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
                transaction: base::TransactionId::new(network.clone(), txid),
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

/// `"{position}:{seq}"`, a null as `0`: its place in its index's legs.
impl crate::db::Keyed for Leg {
    fn key(&self) -> String {
        let (position, seq) = (self.position.unwrap_or(0), self.seq.unwrap_or(0));
        format!("{position}:{seq}")
    }
}

/// A page key: (block position, seq).
pub(crate) type Key = (i64, i64);

/// The key cursor `text` names, as [`Keyed`](crate::db::Keyed) wrote it.
pub(crate) fn parse_key(text: &str) -> Option<Key> {
    let (position, seq) = text.split_once(':')?;
    Some((position.parse().ok()?, seq.parse().ok()?))
}

/// A mined leg with its transaction's txid and state (null for a reward or
/// an adjustment) and its block's time: a row of [`db::find_page`].
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
    /// Its block's time, Unix seconds, when the node reported one.
    #[diesel(sql_type = Nullable<BigInt>)]
    #[schema(required)]
    pub(crate) timestamp: Option<i64>,
}

/// Its leg's key.
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

    /// At most `page.limit` live mined legs of wallet index `index` below
    /// (position, seq) `page.after` when given, latest first, each with its
    /// transaction's txid and state.
    pub(crate) fn find_page(
        connection: &mut SqliteConnection,
        index: impl AsRef<str>,
        page: Page<Key>,
    ) -> QueryResult<Vec<Movement>> {
        // At most `?4` live mined legs of wallet index `?1` below (position
        // `?2`, seq `?3`) (`?2` NULL: from the latest), latest first: by
        // (position, seq), the legs' order, as an adjustment takes the next
        // seq at a lower position; a reward or an adjustment has no
        // transaction.
        diesel::sql_query(
            r#"
            SELECT
                l.*,
                t.txid,
                t.state,
                b.timestamp
            FROM leg l
            JOIN block b ON b.id = l.block
            LEFT JOIN "transaction" t ON t.id = l."transaction"
            WHERE l.wallet_index_id = ?1
              AND (?2 IS NULL OR (l.position, l.seq) < (?2, ?3))
              AND b.reorged_at IS NULL
            ORDER BY l.position DESC, l.seq DESC
            LIMIT ?4"#,
        )
        .bind::<Text, _>(index.as_ref())
        .bind::<Nullable<BigInt>, _>(page.after.map(|(position, _)| position))
        .bind::<BigInt, _>(page.after.map_or(0, |(_, seq)| seq))
        .bind::<BigInt, _>(page.limit)
        .load(connection)
    }

    /// Where each of wallet indexes `indexes` continues from at
    /// `position`: its highest seq ever kept and its totals there, by id.
    ///
    /// # Errors
    ///
    /// When the read fails or a stored total does not decode.
    pub(crate) fn find_running(
        connection: &mut SqliteConnection,
        indexes: &BTreeSet<String>,
        position: i64,
    ) -> anyhow::Result<BTreeMap<String, Running>> {
        #[derive(QueryableByName)]
        struct Row {
            #[diesel(sql_type = Text)]
            id: String,
            #[diesel(sql_type = BigInt)]
            seq: i64,
            #[diesel(sql_type = Text)]
            total: String,
            #[diesel(sql_type = Text)]
            total_received: String,
            #[diesel(sql_type = Text)]
            total_sent: String,
        }
        // Where each wallet index of `?1` (a JSON list of ids) continues from
        // at position `?2`: the highest seq it has ever kept (0: none) and the
        // totals of its last live mined leg at or below `?2` (`last`; '0':
        // none).
        let rows: Vec<Row> = diesel::sql_query(
            "
            SELECT
                i.value AS id,
                (
                    SELECT COALESCE(MAX(seq), 0)
                    FROM leg
                    WHERE wallet_index_id = i.value
                ) AS seq,
                COALESCE(last.total, '0') AS total,
                COALESCE(last.total_received, '0') AS total_received,
                COALESCE(last.total_sent, '0') AS total_sent
            FROM json_each(?1) i
            LEFT JOIN leg last ON last.id = (
                SELECT l.id
                FROM leg l
                JOIN block b ON b.id = l.block
                WHERE l.wallet_index_id = i.value
                  AND l.position <= ?2
                  AND b.reorged_at IS NULL
                ORDER BY l.position DESC, l.seq DESC
                LIMIT 1
            )",
        )
        .bind::<Text, _>(serde_json::to_string(indexes)?)
        .bind::<BigInt, _>(position)
        .load(connection)?;
        let parse = |text: &str| -> anyhow::Result<Decimal> {
            text.parse().context("a stored total does not decode")
        };
        rows.into_iter()
            .map(|row| {
                let running = Running {
                    seq: row.seq,
                    total: parse(&row.total)?,
                    received: parse(&row.total_received)?,
                    sent: parse(&row.total_sent)?,
                };
                Ok((row.id, running))
            })
            .collect()
    }

    /// Deletes the legs of block `block` at `position` kept for wallet
    /// indexes `indexes` (a block kept again writes them again). The
    /// position lets `leg_wallet_index` seek to the block's legs instead of
    /// reading every leg of each index.
    pub(crate) fn delete_in(
        connection: &mut SqliteConnection,
        block: impl AsRef<str>,
        position: i64,
        indexes: &BTreeSet<String>,
    ) -> QueryResult<usize> {
        diesel::delete(
            leg::table
                .filter(leg::wallet_index_id.eq_any(indexes))
                .filter(leg::position.eq(position))
                .filter(leg::block.eq(block.as_ref())),
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

    /// Deletes the mined legs of wallet index `index` above `position`
    /// (a reorged block's too); its pending legs stay. How many.
    pub(crate) fn delete_above(
        connection: &mut SqliteConnection,
        index: impl AsRef<str>,
        position: i64,
    ) -> QueryResult<usize> {
        diesel::delete(
            leg::table
                .filter(leg::wallet_index_id.eq(index.as_ref()))
                .filter(leg::position.gt(position)),
        )
        .execute(connection)
    }

    /// Archives at `at`, a jump's position, every coin wallet index `index`
    /// still shows unspent: its received legs with a coin in a live block,
    /// not archived, that none of its spent legs in a live block names.
    /// What they add up to; `None`: there was none.
    ///
    /// # Errors
    ///
    /// When the write fails or a stored amount does not decode.
    pub(crate) fn archive(
        connection: &mut SqliteConnection,
        index: impl AsRef<str>,
        at: i64,
    ) -> anyhow::Result<Option<Decimal>> {
        #[derive(QueryableByName)]
        struct Archived {
            #[diesel(sql_type = Text)]
            amount: String,
        }
        // Archives at `?2` the unspent coins of wallet index `?1`.
        let rows: Vec<Archived> = diesel::sql_query(
            "
            UPDATE leg
            SET archived_at = ?2
            WHERE wallet_index_id = ?1
              AND kind = 0
              AND coin_txid IS NOT NULL
              AND archived_at IS NULL
              AND EXISTS (
                  SELECT 1
                  FROM block b
                  WHERE b.id = leg.block
                    AND b.reorged_at IS NULL
              )
              AND NOT EXISTS (
                  SELECT 1
                  FROM leg s
                  JOIN block sb ON sb.id = s.block
                  WHERE s.wallet_index_id = leg.wallet_index_id
                    AND s.kind = 1
                    AND s.coin_txid = leg.coin_txid
                    AND s.coin_index = leg.coin_index
                    AND sb.reorged_at IS NULL
              )
            RETURNING amount",
        )
        .bind::<Text, _>(index.as_ref())
        .bind::<BigInt, _>(at)
        .load(connection)?;
        let amounts = rows
            .into_iter()
            .map(|row| {
                row.amount
                    .parse()
                    .context("a stored amount does not decode")
            })
            .collect::<anyhow::Result<Vec<Decimal>>>()?;
        Ok((!amounts.is_empty()).then(|| Decimal::total(&amounts)))
    }

    /// Whether leg `id` is kept.
    pub(crate) fn is_kept(connection: &mut SqliteConnection, id: &str) -> QueryResult<bool> {
        diesel::select(diesel::dsl::exists(leg::table.find(id))).get_result(connection)
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
        block: impl AsRef<str>,
        transaction: impl AsRef<str>,
    ) -> QueryResult<usize> {
        diesel::delete(
            leg::table
                .filter(leg::transaction.eq(transaction.as_ref()))
                .filter(leg::block.is_null().or(leg::block.ne(block.as_ref()))),
        )
        .execute(connection)
    }

    /// How many legs (movements and rewards) every network keeps.
    pub(crate) fn count(connection: &mut SqliteConnection) -> QueryResult<i64> {
        leg::table.count().get_result(connection)
    }

    /// Every unspent coin of `addresses` (all of `network`), as inputs, in
    /// chain order; an archived one is not.
    ///
    /// # Errors
    ///
    /// When the read fails or a stored value does not decode.
    pub(crate) fn find_unspent(
        connection: &mut SqliteConnection,
        network: &Network,
        addresses: &[&str],
    ) -> anyhow::Result<Vec<Input>> {
        // The `Received` legs of an address with a coin, in live blocks, not
        // archived, that no `Spent` leg of the same network consumes, in a
        // live block or in a pending transaction (spent from broadcast on).
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
              AND l.archived_at IS NULL
              AND b.reorged_at IS NULL
              AND NOT EXISTS (
                  SELECT 1
                  FROM leg s
                  JOIN block sb ON sb.id = s.block
                  WHERE s.kind = 1
                    AND s.coin_txid = l.coin_txid
                    AND s.coin_index = l.coin_index
                    AND sb.chain = b.chain
                    AND sb.network = b.network
                    AND sb.reorged_at IS NULL
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
                match row.leg.leg()? {
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
