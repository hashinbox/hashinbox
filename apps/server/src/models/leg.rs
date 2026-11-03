//! `leg`: the kept legs of transactions (no block while pending) and the kept rewards of
//! blocks; a `Received` leg with a coin is a UTXO, spent while a `Spent` leg
//! in a live block names it. A mined leg is its wallet index's and carries
//! the index's running balance (`total`, design 072), in (`position`,
//! `seq`) order.

use anyhow::Context as _;
use base::{Address, Addressed as _, Asset, Coin, Decimal, Input, Network, Reward, TransactionId};
use diesel::{
    prelude::*,
    sql_types::{BigInt, Text},
    sqlite::Sqlite,
};

use super::{child, int, schema::leg, uint, wallet_index::WalletIndex};

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
}

impl Leg {
    /// The row of leg `index` of transaction `transaction`, mined in
    /// `block` or pending (`None`): id `"{transaction}.{index}"`. A mined
    /// row gets its index, position, seq and total from [`db::insert_mined`].
    ///
    /// # Errors
    ///
    /// When an id part holds a dot.
    pub(crate) fn new(
        block: Option<&str>,
        transaction: &str,
        index: u32,
        leg: &base::Leg,
    ) -> anyhow::Result<Self> {
        let (kind, coin) = match leg {
            base::Leg::Received { coin, .. } => (0, coin.as_ref()),
            base::Leg::Spent { coin, .. } => (1, coin.as_ref()),
            base::Leg::Burned { .. } => (2, None),
            base::Leg::Minted { .. } => (3, None),
            base::Leg::Fee { .. } => (4, None),
        };
        Ok(Self {
            id: child(transaction, &[&index.to_string()])?,
            block: block.map(str::to_owned),
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
        })
    }

    /// The row of `reward` of block `block`: id `"{block}.reward.{index}"`.
    /// It gets its index, position, seq and total from [`db::insert_mined`].
    ///
    /// # Errors
    ///
    /// When its index is out of range or an id part holds a dot.
    pub(crate) fn reward(block: &str, reward: &Reward) -> anyhow::Result<Self> {
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
        })
    }

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
    /// Its index's balance right after it: its `total`; zero for a pending
    /// leg (none).
    ///
    /// # Errors
    ///
    /// When the stored total does not decode.
    pub(crate) fn balance(&self) -> anyhow::Result<Decimal> {
        self.total.as_deref().map_or_else(
            || Ok(Decimal::zero()),
            |total| total.parse().context("a stored total does not decode"),
        )
    }

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

collection! {
    /// Legs, in the order read or written.
    Legs(Leg)
}

impl Legs {
    /// What they add to their addresses' balances.
    ///
    /// # Errors
    ///
    /// When a stored amount or kind does not decode.
    pub(crate) fn sum(&self) -> anyhow::Result<Decimal> {
        self.iter()
            .try_fold(Decimal::zero(), |sum, leg| Ok(sum + leg.signed()?))
    }
}

/// The `leg` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    /// A coin with its creating block's height.
    #[derive(QueryableByName)]
    struct Unspent {
        #[diesel(embed)]
        leg: Leg,
        #[diesel(sql_type = BigInt)]
        height: i64,
    }

    /// The `Received` legs of an address with a coin, in live blocks, that no
    /// `Spent` leg of the same network consumes, in a live block or in a
    /// pending transaction (spent from broadcast on).
    const UNSPENT: &str = r#"
    SELECT l.*, b.height AS height FROM leg l
    JOIN block b ON b.id = l.block
    JOIN "transaction" t ON t.id = l."transaction"
    WHERE b.chain = ?1 AND b.network = ?2
      AND l.address IN (SELECT value FROM json_each(?3))
      AND l.kind = 0 AND l.coin_txid IS NOT NULL AND b.archived_at IS NULL
      AND NOT EXISTS (
        SELECT 1 FROM leg s JOIN block sb ON sb.id = s.block
        WHERE s.kind = 1 AND s.coin_txid = l.coin_txid AND s.coin_index = l.coin_index
          AND sb.chain = b.chain AND sb.network = b.network AND sb.archived_at IS NULL)
      AND NOT EXISTS (
        SELECT 1 FROM leg s JOIN "transaction" st ON st.id = s."transaction"
        WHERE s.kind = 1 AND s.coin_txid = l.coin_txid AND s.coin_index = l.coin_index
          AND s.block IS NULL AND st.block IS NULL
          AND st.chain = b.chain AND st.network = b.network)
    ORDER BY b.position, t."order", l.leg_index"#;

    /// A wallet index's last live mined leg at or below a position (one
    /// seek on `leg_wallet_index`).
    const LAST: &str = "
    SELECT l.* FROM leg l JOIN block b ON b.id = l.block
    WHERE l.wallet_index_id = ?1 AND l.position <= ?2 AND b.archived_at IS NULL
    ORDER BY l.position DESC, l.seq DESC LIMIT 1";

    /// The highest seq a wallet index has kept (0: none).
    const SEQ: &str = "SELECT COALESCE(MAX(seq), 0) AS seq FROM leg WHERE wallet_index_id = ?1";

    /// A wallet index's highest seq.
    #[derive(QueryableByName)]
    struct Seq {
        #[diesel(sql_type = BigInt)]
        seq: i64,
    }

    /// An address's outgoing legs (spent, burned, fee) in an asset in
    /// pending transactions.
    /// Wallet index `index`'s last live mined leg at or below `through`
    /// (`i64::MAX`: the last): its `total` is the index's balance there.
    pub(crate) fn find_last(
        connection: &mut SqliteConnection,
        index: &str,
        through: i64,
    ) -> QueryResult<Option<Leg>> {
        diesel::sql_query(LAST)
            .bind::<Text, _>(index)
            .bind::<BigInt, _>(through)
            .get_result(connection)
            .optional()
    }

    /// Keeps mined `row` (of its block, at `position`) as the next leg of
    /// wallet index `index`: its seq after the highest the index has kept,
    /// its total the index's last live leg at or below `position` (legs of
    /// the block written before it included) plus its amount. A seq the
    /// index already holds is refused (`leg_seq`): two writers of one step
    /// cannot both land.
    ///
    /// # Errors
    ///
    /// When a stored or given amount does not decode, or a write fails.
    pub(crate) fn insert_mined(
        connection: &mut SqliteConnection,
        mut row: Leg,
        position: i64,
    ) -> anyhow::Result<()> {
        let index = row.wallet_index_id.clone();
        let index = index.as_str();
        let before = match find_last(connection, index, position)? {
            Some(last) => last.balance()?,
            None => Decimal::zero(),
        };
        let total = before + row.signed()?;
        let last: Seq = diesel::sql_query(SEQ)
            .bind::<Text, _>(index)
            .get_result(connection)?;
        row.position = Some(position);
        row.seq = Some(last.seq + 1);
        row.total = Some(total.to_string());
        diesel::insert_into(leg::table)
            .values(&row)
            .execute(connection)?;
        Ok(())
    }

    /// Deletes the legs of block `block` kept for `indexes` (a block kept
    /// again writes them again).
    pub(crate) fn delete_in(
        connection: &mut SqliteConnection,
        block: &str,
        indexes: &[String],
    ) -> QueryResult<usize> {
        diesel::delete(
            leg::table
                .filter(leg::block.eq(block))
                .filter(leg::wallet_index_id.eq_any(indexes)),
        )
        .execute(connection)
    }

    /// Whether `address` can spend `takes` (signed: what a send takes is
    /// negative) of `asset`: its index's balance now (zero without an
    /// index: no mined leg is kept) less what its pending sends take stays
    /// at or above zero.
    ///
    /// # Errors
    ///
    /// When a read fails, a stored amount does not decode, or an id part
    /// holds a dot.
    pub(crate) fn can_spend(
        connection: &mut SqliteConnection,
        address: &Address,
        asset: &Asset,
        takes: Decimal,
    ) -> anyhow::Result<bool> {
        let index = WalletIndex::id(address, asset)?;
        let now = match find_last(connection, &index, i64::MAX)? {
            Some(last) => last.balance()?,
            None => Decimal::zero(),
        };
        let pending = find_pending_out(connection, &index)?.sum()?;
        Ok(now + pending + takes >= Decimal::zero())
    }

    /// The pending outgoing legs (spent, burned, fee) of wallet index
    /// `index`: what its pending sends take until mined, refused or expired.
    fn find_pending_out(connection: &mut SqliteConnection, index: &str) -> QueryResult<Legs> {
        leg::table
            .filter(leg::wallet_index_id.eq(index))
            .filter(leg::block.is_null())
            .filter(leg::kind.eq_any([1, 2, 4]))
            .select(Leg::as_select())
            .load::<Leg>(connection)
            .map(Legs::from)
    }

    /// Keeps pending `rows`; a leg already kept is kept once.
    pub(crate) fn insert(connection: &mut SqliteConnection, rows: &Legs) -> QueryResult<usize> {
        // A kept leg is skipped; a row breaking a constraint is an error,
        // never silently dropped (as `INSERT OR IGNORE` would).
        // One statement per row: diesel has no batch upsert on SQLite.
        rows.iter().try_fold(0, |added, row| {
            diesel::insert_into(leg::table)
                .values(row)
                .on_conflict_do_nothing()
                .execute(connection)
                .map(|one| added + one)
        })
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

    /// Deletes the legs of transaction `transaction` in no block (pending).
    pub(crate) fn delete_for(
        connection: &mut SqliteConnection,
        transaction: &str,
    ) -> QueryResult<usize> {
        diesel::delete(
            leg::table
                .filter(leg::transaction.eq(transaction))
                .filter(leg::block.is_null()),
        )
        .execute(connection)
    }

    /// How many legs (movements and rewards) every network keeps.
    pub(crate) fn count(connection: &mut SqliteConnection) -> QueryResult<i64> {
        leg::table.count().get_result(connection)
    }

    /// The legs of transaction `transaction`, in order.
    pub(crate) fn find_for(
        connection: &mut SqliteConnection,
        transaction: &str,
    ) -> QueryResult<Legs> {
        leg::table
            .filter(leg::transaction.eq(transaction))
            .order(leg::leg_index)
            .select(Leg::as_select())
            .load::<Leg>(connection)
            .map(Legs::from)
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
        let rows: Vec<Unspent> = diesel::sql_query(UNSPENT)
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
