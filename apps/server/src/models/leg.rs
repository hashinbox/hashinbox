//! `leg`: the kept legs of transactions (no block while pending) and the kept rewards of
//! blocks; a `Received` leg with a coin is a UTXO, spent while a `Spent` leg
//! in a live block names it.

use anyhow::Context as _;
use base::{Address, Addressed as _, Asset, Coin, Decimal, Input, Network, Reward, TransactionId};
use diesel::{
    prelude::*,
    sql_types::{BigInt, Text},
    sqlite::Sqlite,
};

use super::{child, int, schema::leg, uint};

/// The `kind` of a reward row.
const REWARD: i32 = 5;

/// One `leg` row: a transaction's leg, or a block's reward.
#[derive(Clone, Debug, Queryable, Selectable, Insertable, QueryableByName)]
#[diesel(table_name = leg, check_for_backend(Sqlite))]
pub(crate) struct Leg {
    pub(crate) id: String,
    pub(crate) block: Option<String>,
    pub(crate) transaction: Option<String>,
    pub(crate) leg_index: i64,
    pub(crate) kind: i32,
    pub(crate) address: String,
    pub(crate) asset: String,
    pub(crate) amount: String,
    pub(crate) coin_txid: Option<String>,
    pub(crate) coin_index: Option<i64>,
}

impl Leg {
    /// The row of leg `index` of transaction `transaction`, mined in
    /// `block` or pending (`None`): id `"{transaction}.{index}"`.
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
        })
    }

    /// The row of `reward` of block `block`: id `"{block}.reward.{index}"`.
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

    /// The legs (rewards included) of an address in an asset, in live blocks.
    const LIVE: &str = "
    SELECT l.* FROM leg l JOIN block b ON b.id = l.block
    WHERE b.chain = ?1 AND b.network = ?2 AND l.address = ?3 AND l.asset = ?4
      AND b.archived_at IS NULL";

    /// What an address can spend of an asset: its legs in live blocks, and
    /// its outgoing legs (spent, burned, fee) in pending transactions.
    const SPENDABLE: &str = r#"
    SELECT l.* FROM leg l JOIN block b ON b.id = l.block
    WHERE b.chain = ?1 AND b.network = ?2 AND l.address = ?3 AND l.asset = ?4
      AND b.archived_at IS NULL
    UNION ALL
    SELECT l.* FROM leg l JOIN "transaction" t ON t.id = l."transaction"
    WHERE t.chain = ?1 AND t.network = ?2 AND l.address = ?3 AND l.asset = ?4
      AND t.block IS NULL AND l.block IS NULL AND l.kind IN (1, 2, 4)"#;

    /// The legs `address` can spend `asset` by: [`SPENDABLE`]. Their sum
    /// is what it can spend now.
    pub(crate) fn find_spendable(
        connection: &mut SqliteConnection,
        address: &Address,
        asset: &Asset,
    ) -> QueryResult<Legs> {
        diesel::sql_query(SPENDABLE)
            .bind::<Text, _>(address.network.chain.as_str())
            .bind::<Text, _>(&address.network.name)
            .bind::<Text, _>(&address.value)
            .bind::<Text, _>(&asset.id)
            .load::<Leg>(connection)
            .map(Legs::from)
    }

    /// Keeps `rows`; a leg already kept is kept once.
    pub(crate) fn insert(connection: &mut SqliteConnection, rows: &Legs) -> QueryResult<usize> {
        diesel::insert_or_ignore_into(leg::table)
            .values(&rows[..])
            .execute(connection)
    }

    /// Keeps `rows`, legs of transaction `transaction` mined in `block`, in
    /// place of its legs of any other block or none (pending); its legs
    /// already kept in `block` (other indexed pairs) stay.
    pub(crate) fn replace(
        connection: &mut SqliteConnection,
        block: &str,
        transaction: &str,
        rows: &Legs,
    ) -> QueryResult<usize> {
        diesel::delete(
            leg::table
                .filter(leg::transaction.eq(transaction))
                .filter(leg::block.is_null().or(leg::block.ne(block))),
        )
        .execute(connection)?;
        insert(connection, rows)
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

    /// Every leg of `address` in `asset` in a live block, rewards included:
    /// what its balance is the sum of.
    pub(crate) fn find_live(
        connection: &mut SqliteConnection,
        address: &Address,
        asset: &Asset,
    ) -> QueryResult<Legs> {
        diesel::sql_query(LIVE)
            .bind::<Text, _>(address.network.chain.as_str())
            .bind::<Text, _>(&address.network.name)
            .bind::<Text, _>(&address.value)
            .bind::<Text, _>(&asset.id)
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
