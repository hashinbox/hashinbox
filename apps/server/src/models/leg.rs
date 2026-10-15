//! `leg`: the kept legs of transactions (no block while pending) and the kept rewards of
//! blocks; a `Received` leg with a coin is a UTXO, spent while a `Spent` leg
//! in a live block names it.

use anyhow::Context as _;
use base::{Address, Asset, Coin, Decimal, Input, Leg, Network, Reward, TransactionId};
use diesel::{
    prelude::*,
    sql_types::{BigInt, Text},
    sqlite::Sqlite,
};

use super::{child, int, schema::leg, uint};

/// The `kind` of a reward row.
const REWARD: i32 = 5;

/// One `leg` row.
#[derive(Clone, Debug, Queryable, Selectable, Insertable, QueryableByName)]
#[diesel(table_name = leg, check_for_backend(Sqlite))]
pub(crate) struct LegRow {
    pub(crate) id: String,
    pub(crate) block: Option<String>,
    pub(crate) transaction: Option<String>,
    #[diesel(embed)]
    pub(crate) facts: LegFacts,
}

/// What one leg moved: a `leg` row's own columns.
#[derive(Clone, Debug, Queryable, Selectable, Insertable, QueryableByName)]
#[diesel(table_name = leg, check_for_backend(Sqlite))]
pub(crate) struct LegFacts {
    pub(crate) leg_index: i64,
    pub(crate) kind: i32,
    pub(crate) address: String,
    pub(crate) asset: String,
    pub(crate) amount: String,
    pub(crate) coin_txid: Option<String>,
    pub(crate) coin_index: Option<i64>,
}

/// A coin with its creating block's height.
#[derive(QueryableByName)]
struct Unspent {
    #[diesel(embed)]
    leg: LegFacts,
    #[diesel(sql_type = BigInt)]
    height: i64,
}

/// The `Received` legs of an address with a coin, in live blocks, that no
/// `Spent` leg in a live block of the same network consumes.
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
ORDER BY b.position, t."order", l.leg_index"#;

/// The legs (rewards included) of an address in an asset, in live blocks.
const LIVE: &str = "
SELECT l.* FROM leg l JOIN block b ON b.id = l.block
WHERE b.chain = ?1 AND b.network = ?2 AND l.address = ?3 AND l.asset = ?4
  AND b.archived_at IS NULL";

impl LegRow {
    /// The row of leg `index` of transaction `transaction`, mined in
    /// `block` or pending (`None`): id `"{transaction}.{index}"`.
    ///
    /// # Errors
    ///
    /// When the coin index is out of range or an id part holds a dot.
    pub(crate) fn new(
        block: Option<&str>,
        transaction: &str,
        index: u32,
        leg: &Leg,
    ) -> anyhow::Result<Self> {
        let facts = LegFacts::new(index, leg);
        Ok(Self {
            id: child(transaction, &[&facts.leg_index.to_string()])?,
            block: block.map(str::to_owned),
            transaction: Some(transaction.to_owned()),
            facts,
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
            facts: LegFacts {
                leg_index: index,
                kind: REWARD,
                address: reward.address.value.clone(),
                asset: reward.asset.id.clone(),
                amount: reward.amount.to_string(),
                coin_txid: None,
                coin_index: None,
            },
        })
    }

    /// Keeps `rows`; a leg already kept is kept once.
    pub(crate) fn insert(connection: &mut SqliteConnection, rows: &[Self]) -> QueryResult<usize> {
        diesel::insert_or_ignore_into(leg::table)
            .values(rows)
            .execute(connection)
    }

    /// Keeps `rows`, legs of transaction `transaction` mined in `block`, in
    /// place of its legs of any other block or none (pending); its legs
    /// already kept in `block` (other indexed pairs) stay.
    pub(crate) fn replace(
        connection: &mut SqliteConnection,
        block: &str,
        transaction: &str,
        rows: &[Self],
    ) -> QueryResult<usize> {
        diesel::delete(
            leg::table
                .filter(leg::transaction.eq(transaction))
                .filter(leg::block.is_null().or(leg::block.ne(block))),
        )
        .execute(connection)?;
        Self::insert(connection, rows)
    }

    /// The legs of transaction `transaction`, in order.
    pub(crate) fn find_for(
        connection: &mut SqliteConnection,
        transaction: &str,
    ) -> QueryResult<Vec<LegFacts>> {
        leg::table
            .filter(leg::transaction.eq(transaction))
            .order(leg::leg_index)
            .select(LegFacts::as_select())
            .load(connection)
    }

    /// Every leg of `address` in `asset` in a live block, rewards included:
    /// what its balance is the sum of.
    pub(crate) fn find_live(
        connection: &mut SqliteConnection,
        address: &Address,
        asset: &Asset,
    ) -> QueryResult<Vec<LegFacts>> {
        diesel::sql_query(LIVE)
            .bind::<Text, _>(address.network.chain.as_str())
            .bind::<Text, _>(&address.network.name)
            .bind::<Text, _>(&address.value)
            .bind::<Text, _>(&asset.id)
            .load(connection)
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
                    Leg::Received {
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

impl LegFacts {
    /// The facts of transaction leg `index`.
    pub(crate) fn new(index: u32, leg: &Leg) -> Self {
        let (kind, address, asset, amount, coin) = match leg {
            Leg::Received {
                address,
                asset,
                amount,
                coin,
            } => (0, address, asset, amount, coin.as_ref()),
            Leg::Spent {
                address,
                asset,
                amount,
                coin,
            } => (1, address, asset, amount, coin.as_ref()),
            Leg::Burned {
                address,
                asset,
                amount,
            } => (2, address, asset, amount, None),
            Leg::Minted {
                address,
                asset,
                amount,
            } => (3, address, asset, amount, None),
            Leg::Fee {
                address,
                asset,
                amount,
            } => (4, address, asset, amount, None),
        };
        Self {
            leg_index: i64::from(index),
            kind,
            address: address.value.clone(),
            asset: asset.id.clone(),
            amount: amount.to_string(),
            coin_txid: coin.map(|coin| coin.transaction.id.clone()),
            coin_index: coin.map(|coin| i64::from(coin.index)),
        }
    }

    /// What it adds to its address's balance: credits (received, minted, a
    /// signed reward) count up, debits (spent, burned, fees) down.
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

    /// The transaction leg of these facts, of `network`.
    ///
    /// # Errors
    ///
    /// When a stored value does not decode, or it is a reward.
    pub(crate) fn leg(self, network: &Network) -> anyhow::Result<Leg> {
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
            0 => Leg::Received {
                address,
                asset,
                amount,
                coin,
            },
            1 => Leg::Spent {
                address,
                asset,
                amount,
                coin,
            },
            2 => Leg::Burned {
                address,
                asset,
                amount,
            },
            3 => Leg::Minted {
                address,
                asset,
                amount,
            },
            4 => Leg::Fee {
                address,
                asset,
                amount,
            },
            _ => anyhow::bail!("a stored leg kind is not a transaction leg"),
        })
    }
}
