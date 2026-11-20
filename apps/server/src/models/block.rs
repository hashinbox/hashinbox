//! `block`: every block kept, of every network.

use std::collections::BTreeSet;

use base::{BlockHash, BlockHeader, BlockParent, Chain, Network, Networked};
use diesel::{
    deserialize::FromSqlRow,
    expression::AsExpression,
    prelude::*,
    sql_types::{BigInt, Text},
    sqlite::Sqlite,
};

use super::{
    Id, IdError, Leg, id::typed_id, int, leg::Legs, schema::block, transaction::Transaction, uint,
};

/// A block's id: `{chain}.{network}.{hash in hex}`.
#[derive(Clone, Debug, PartialEq, Eq, AsExpression, FromSqlRow)]
#[diesel(sql_type = Text)]
pub(crate) struct BlockId {
    pub(crate) network: Network,
    pub(crate) hash: BlockHash,
    id: Id,
}

impl BlockId {
    /// The id of `network`'s block `hash`.
    ///
    /// # Errors
    ///
    /// When a part holds a dot.
    pub(crate) fn new(network: &Network, hash: &BlockHash) -> Result<Self, IdError> {
        let id = Id::new(&[network.chain.as_str(), &network.name, &hex::encode(&hash.0)])?;
        Ok(Self {
            network: network.clone(),
            hash: hash.clone(),
            id,
        })
    }

    /// `text` as a block id.
    ///
    /// # Errors
    ///
    /// Unless it has its three parts, the hash in hex.
    pub(crate) fn parse(text: &str) -> Result<Self, IdError> {
        let [chain, network, hash] = Id::parts::<3>(text, "{chain}.{network}.{hash}")?;
        let hash = hex::decode(hash)
            .map_err(|error| IdError(format!("a block id's hash is not hex: {error}")))?;
        Self::new(&Network::new(Chain::new(chain), network), &BlockHash(hash))
    }
}

typed_id!(BlockId, network);

/// Serializes bytes as lowercase hexadecimal.
fn hex_text<S: serde::Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&hex::encode(bytes))
}

/// A kept block: its `block` row; the API shows it as is. Written with its
/// kept transactions in block order and its kept rewards, which are never
/// read back into it.
#[derive(Clone, Debug, Insertable, serde::Serialize, utoipa::ToSchema)]
#[diesel(table_name = block, check_for_backend(Sqlite))]
pub(crate) struct Block {
    /// `{chain}.{network}.{hash}`.
    #[schema(value_type = String)]
    pub(crate) id: BlockId,
    pub(crate) chain: String,
    pub(crate) network: String,
    /// Native position (height or slot).
    pub(crate) position: i64,
    /// Produced-block count.
    pub(crate) height: i64,
    /// Block hash, hexadecimal.
    #[serde(serialize_with = "hex_text")]
    #[schema(value_type = String)]
    pub(crate) hash: Vec<u8>,
    /// Its parent's id, once that is kept.
    #[schema(value_type = Option<String>)]
    pub(crate) parent: Option<BlockId>,
    /// Its parent's position; null for genesis.
    pub(crate) parent_position: Option<i64>,
    #[serde(skip)]
    pub(crate) parent_hash: Option<Vec<u8>>,
    /// Unix seconds, when reported.
    pub(crate) timestamp: Option<i64>,
    /// Its kept transactions, in block order; written, not read.
    #[serde(skip)]
    #[diesel(skip_insertion)]
    pub(crate) transactions: Vec<Transaction>,
    /// Its kept rewards, in order; written, not read.
    #[serde(skip)]
    #[diesel(skip_insertion)]
    pub(crate) rewards: Vec<Leg>,
}

/// A row as the header it was written from. Fails when a stored integer is
/// negative.
impl TryFrom<&Block> for BlockHeader {
    type Error = anyhow::Error;

    fn try_from(row: &Block) -> anyhow::Result<Self> {
        let parent = match (row.parent_position, &row.parent_hash) {
            (Some(position), Some(hash)) => Some(BlockParent {
                position: uint(position)?,
                hash: BlockHash(hash.clone()),
            }),
            _ => None,
        };
        Ok(Self {
            position: uint(row.position)?,
            height: uint(row.height)?,
            hash: BlockHash(row.hash.clone()),
            parent,
            timestamp: row.timestamp.map(uint).transpose()?,
        })
    }
}

/// `block` as kept, with every transaction (sorted by order) and
/// reward (sorted by index), all mined in it; its parent not linked yet
/// ([`db::save`] links it).
impl TryFrom<&base::Block> for Block {
    type Error = anyhow::Error;

    fn try_from(block: &base::Block) -> anyhow::Result<Self> {
        let header = &block.header;
        let id = BlockId::new(&block.network, &header.hash)?;
        let position = int(header.position)?;
        let mut transactions = block
            .transactions
            .iter()
            .map(Transaction::try_from)
            .collect::<anyhow::Result<Vec<_>>>()?;
        transactions.sort_by_key(|transaction| transaction.order.unwrap_or(0));
        for transaction in &mut transactions {
            transaction.block = Some(id.clone());
            transaction.position = Some(position);
            for leg in &mut transaction.legs {
                leg.block = Some(id.clone());
                leg.position = Some(position);
            }
        }
        let mut rewards = block
            .rewards
            .iter()
            .map(|reward| Leg::try_from((&id, reward)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        rewards.sort_by_key(|reward| reward.leg_index);
        for reward in &mut rewards {
            reward.position = Some(position);
        }
        Ok(Self {
            chain: block.network.chain.to_string(),
            network: block.network.name.clone(),
            position,
            height: int(header.height)?,
            hash: header.hash.0.clone(),
            parent: None,
            parent_position: header
                .parent
                .as_ref()
                .map(|parent| int(parent.position))
                .transpose()?,
            parent_hash: header.parent.as_ref().map(|parent| parent.hash.0.clone()),
            timestamp: header.timestamp.map(int).transpose()?,
            transactions,
            rewards,
            id,
        })
    }
}

impl Networked for Block {
    fn network(&self) -> &Network {
        self.id.network()
    }
}

impl Block {
    /// The ids of its transactions.
    pub(crate) fn txids(&self) -> Vec<&str> {
        self.transactions
            .iter()
            .map(|transaction| transaction.id.as_ref())
            .collect()
    }

    /// The wallet indexes (ids) the legs of its transactions `watched` are
    /// of.
    pub(crate) fn indexes_of(&self, watched: &BTreeSet<String>) -> BTreeSet<String> {
        self.transactions
            .iter()
            .filter(|transaction| watched.contains(transaction.id.as_ref()))
            .flat_map(|transaction| &transaction.legs)
            .map(|leg| leg.wallet_index_id.clone().into())
            .collect()
    }

    /// Drops its legs of wallet indexes not in `ours` (index ids), but a
    /// `watched` transaction's (ids: a transaction index found them in it)
    /// of an index `behind` it too (paused, say: a send from it is kept
    /// mined); then its transactions left without a leg, but those
    /// `watched`.
    pub(crate) fn keep(
        &mut self,
        ours: &BTreeSet<String>,
        watched: &BTreeSet<String>,
        behind: &BTreeSet<String>,
    ) {
        let both = ours.union(behind).cloned().collect();
        for transaction in &mut self.transactions {
            let found = watched.contains(transaction.id.as_ref());
            transaction.keep(if found { &both } else { ours });
        }
        self.transactions.retain(|transaction| {
            !transaction.legs.is_empty() || watched.contains(transaction.id.as_ref())
        });
        self.rewards
            .retain(|reward| ours.contains(reward.wallet_index_id.as_ref()));
    }

    /// Takes its legs out: each transaction's in order, then its rewards
    /// (072's order within a block).
    pub(crate) fn take_legs(&mut self) -> Legs {
        self.transactions
            .iter_mut()
            .flat_map(|transaction| std::mem::take(&mut transaction.legs))
            .chain(std::mem::take(&mut self.rewards))
            .collect()
    }
}

/// Its position.
impl crate::db::Keyed for Block {
    fn key(&self) -> String {
        self.position.to_string()
    }
}

/// The `block` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;
    use crate::db::Page;

    /// One `block` row.
    #[derive(Queryable, QueryableByName, Selectable)]
    #[diesel(table_name = block, check_for_backend(Sqlite))]
    pub(in crate::models) struct Row {
        id: BlockId,
        chain: String,
        network: String,
        position: i64,
        height: i64,
        hash: Vec<u8>,
        parent: Option<BlockId>,
        parent_position: Option<i64>,
        parent_hash: Option<Vec<u8>>,
        timestamp: Option<i64>,
    }

    /// The row, transactions and rewards not read.
    impl From<Row> for Block {
        fn from(row: Row) -> Self {
            Self {
                id: row.id,
                chain: row.chain,
                network: row.network,
                position: row.position,
                height: row.height,
                hash: row.hash,
                parent: row.parent,
                parent_position: row.parent_position,
                parent_hash: row.parent_hash,
                timestamp: row.timestamp,
                transactions: Vec::new(),
                rewards: Vec::new(),
            }
        }
    }

    /// The blocks of `network`.
    macro_rules! of {
        ($network:expr) => {
            block::table
                .filter(block::chain.eq($network.chain.as_str()))
                .filter(block::network.eq($network.name.as_str()))
        };
    }

    /// Keeps `block`'s row, no index moved and none of its facts written:
    /// inserts it linked to its kept parent and links a kept child to it;
    /// one with its id already kept is a no-op.
    ///
    /// # Errors
    ///
    /// When another block of its network is kept at its position, a value
    /// does not decode or a write fails.
    pub(crate) fn save(connection: &mut SqliteConnection, block: &mut Block) -> anyhow::Result<()> {
        let parent = block
            .parent_hash
            .clone()
            .map(|hash| BlockId::new(block.network(), &BlockHash(hash)))
            .transpose()?;
        block.parent = match &parent {
            Some(parent) => block::table
                .find(parent)
                .select(block::id)
                .first::<BlockId>(connection)
                .optional()?,
            None => None,
        };
        let added = diesel::insert_into(block::table)
            .values(&*block)
            .on_conflict(block::id)
            .do_nothing()
            .execute(connection);
        let added = match added {
            Err(diesel::result::Error::DatabaseError(
                diesel::result::DatabaseErrorKind::UniqueViolation,
                _,
            )) => anyhow::bail!(
                "{:?}: block {} conflicts with the kept one",
                block.network(),
                block.position
            ),
            added => added?,
        };
        if added == 0 {
            return Ok(());
        }
        diesel::update(
            block::table
                .filter(block::chain.eq(&block.chain))
                .filter(block::network.eq(&block.network))
                .filter(block::parent.is_null())
                .filter(block::parent_hash.eq(&block.hash)),
        )
        .set(block::parent.eq(&block.id))
        .execute(connection)?;
        Ok(())
    }

    /// At most `page.limit` blocks of `network` below position
    /// `page.after` when given, latest first.
    pub(crate) fn find_page(
        connection: &mut SqliteConnection,
        network: &Network,
        page: Page<i64>,
    ) -> QueryResult<Vec<Block>> {
        let mut query = of!(network).into_boxed();
        if let Some(before) = page.after {
            query = query.filter(block::position.lt(before));
        }
        query
            .order(block::position.desc())
            .limit(page.limit)
            .select(Row::as_select())
            .load(connection)
            .map(|rows| rows.into_iter().map(Block::from).collect())
    }

    /// The block of `network` at `position`.
    pub(crate) fn find_by_position(
        connection: &mut SqliteConnection,
        network: &Network,
        position: i64,
    ) -> QueryResult<Option<Block>> {
        of!(network)
            .filter(block::position.eq(position))
            .select(Row::as_select())
            .first(connection)
            .optional()
            .map(|row| row.map(Block::from))
    }

    /// How many blocks of every network are kept.
    pub(crate) fn count(connection: &mut SqliteConnection) -> QueryResult<i64> {
        block::table.count().get_result(connection)
    }

    /// The stamp (Unix seconds) of the newest block of any network.
    pub(crate) fn find_newest_stamp(connection: &mut SqliteConnection) -> QueryResult<Option<i64>> {
        block::table
            .select(diesel::dsl::max(block::timestamp))
            .first(connection)
    }

    /// How many blocks of every network are stamped in each hour from
    /// `since` (Unix seconds) on, as (hour, count); hour 0 starts at
    /// `since`.
    pub(crate) fn count_by_hour(
        connection: &mut SqliteConnection,
        since: i64,
    ) -> QueryResult<Vec<(i64, i64)>> {
        block::table
            .filter(block::timestamp.ge(since))
            .group_by(diesel::dsl::sql::<BigInt>("hour"))
            .select((
                diesel::dsl::sql::<BigInt>("(timestamp - ")
                    .bind::<BigInt, _>(since)
                    .sql(") / 3600 AS hour"),
                diesel::dsl::count_star(),
            ))
            .load(connection)
    }

    /// The block of `network` right below `position`.
    pub(crate) fn find_below(
        connection: &mut SqliteConnection,
        network: &Network,
        position: i64,
    ) -> QueryResult<Option<Block>> {
        of!(network)
            .filter(block::position.lt(position))
            .order(block::position.desc())
            .select(Row::as_select())
            .first(connection)
            .optional()
            .map(|row| row.map(Block::from))
    }

    /// The height of `network`'s newest kept block at or below `position`;
    /// `None`: none kept.
    pub(crate) fn find_height_at_or_below(
        connection: &mut SqliteConnection,
        network: &Network,
        position: i64,
    ) -> QueryResult<Option<i64>> {
        of!(network)
            .filter(block::position.le(position))
            .order(block::position.desc())
            .select(block::height)
            .first(connection)
            .optional()
    }

    /// Deletes every block of `network` at or above `from` (a reorg's: the
    /// transactions and legs there are taken back first, but rewards, which
    /// go with it by cascade), and a kept child's `parent` is set NULL.
    /// How many.
    pub(crate) fn delete_from(
        connection: &mut SqliteConnection,
        network: &Network,
        from: i64,
    ) -> QueryResult<usize> {
        diesel::delete(of!(network).filter(block::position.ge(from))).execute(connection)
    }

    /// Deletes the blocks of `network` below position `below` that nothing
    /// needs: no leg, no transaction; the newest block always stays (the
    /// indexer checks it is still on the chain). Never a block with
    /// history: nothing cascades. How many.
    pub(crate) fn prune(
        connection: &mut SqliteConnection,
        network: &Network,
        below: i64,
    ) -> QueryResult<usize> {
        diesel::sql_query(
            r#"
            DELETE FROM block
            WHERE chain = ?1
              AND network = ?2
              AND position < ?3
              AND position < (
                  SELECT MAX(newest.position)
                  FROM block AS newest
                  WHERE newest.chain = ?1
                    AND newest.network = ?2
              )
              AND NOT EXISTS (SELECT 1 FROM leg WHERE leg.block = block.id)
              AND NOT EXISTS (SELECT 1 FROM "transaction" AS t WHERE t.block = block.id)"#,
        )
        .bind::<Text, _>(network.chain.as_str())
        .bind::<Text, _>(&network.name)
        .bind::<BigInt, _>(below)
        .execute(connection)
    }
}
