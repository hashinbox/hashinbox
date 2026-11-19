//! `block`: every block kept, live or reorged, of every network.

use std::collections::BTreeSet;

use anyhow::Context as _;
use base::{BlockHash, BlockHeader, BlockParent, Chain, Network, Networked};
use diesel::{
    deserialize::FromSqlRow,
    expression::AsExpression,
    prelude::*,
    sql_types::{BigInt, Text},
    sqlite::Sqlite,
};

use super::{
    Id, IdError, Leg, id::typed_id, int, leg, schema::block, transaction, transaction::Transaction,
    transaction_index, uint, wallet_index,
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
    /// Unix ms a reorg took it back; null while live.
    pub(crate) reorged_at: Option<i64>,
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

/// `block` as kept live, with every transaction (sorted by order) and
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
            reorged_at: None,
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
    /// The block `header` names, with no transactions or rewards read: what
    /// a settle points its index at.
    ///
    /// # Errors
    ///
    /// When a part of its id holds a dot, or a position is out of range.
    pub(crate) fn empty(network: &Network, header: &BlockHeader) -> anyhow::Result<Self> {
        Self::try_from(&base::Block {
            network: network.clone(),
            header: header.clone(),
            transactions: base::Transactions::default(),
            rewards: Vec::new(),
        })
    }

    /// Whether this row is `header`'s block.
    pub(crate) fn is(&self, header: &BlockHeader) -> bool {
        BlockHeader::try_from(self).is_ok_and(|own| own == *header)
    }

    /// Whether `header` names this block as its parent.
    pub(crate) fn is_parent_of(&self, header: &BlockHeader) -> bool {
        header.parent.as_ref().is_some_and(|parent| {
            int(parent.position).is_ok_and(|position| position == self.position)
                && parent.hash.0 == self.hash
        })
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

    /// Its legs: each transaction's in order, then its rewards (072's
    /// order within a block).
    fn legs(&self) -> impl Iterator<Item = &Leg> {
        self.transactions
            .iter()
            .flat_map(|transaction| &transaction.legs)
            .chain(&self.rewards)
    }
}

/// Its position.
impl crate::db::Keyed for Block {
    fn key(&self) -> String {
        self.position.to_string()
    }
}

collection! {
    /// Live blocks of one network, ordered by position.
    Blocks(Block)
}

/// Where a block would go among [`Blocks`].
pub(crate) enum Place<'a> {
    /// This very block is already kept there.
    Held(&'a Block),
    /// Another live block is there.
    Taken,
    /// Nothing is there.
    Free,
}

impl Blocks {
    /// Where `header` would go.
    ///
    /// # Errors
    ///
    /// When its position is out of range.
    pub(crate) fn place(&self, header: &BlockHeader) -> anyhow::Result<Place<'_>> {
        let position = int(header.position)?;
        Ok(match self.at(position) {
            Some(held) if held.is(header) => Place::Held(held),
            Some(_) => Place::Taken,
            None => Place::Free,
        })
    }

    /// The block at `position`.
    pub(crate) fn at(&self, position: i64) -> Option<&Block> {
        let found = self
            .0
            .binary_search_by_key(&position, |block| block.position);
        found.ok().map(|at| &self.0[at])
    }

    /// The nearest block below `position`.
    pub(crate) fn below(&self, position: i64) -> Option<&Block> {
        self.0.iter().rev().find(|block| block.position < position)
    }

    /// The nearest block above `position`.
    pub(crate) fn above(&self, position: i64) -> Option<&Block> {
        self.0.iter().find(|block| block.position > position)
    }

    /// Adds `block` in order; one at its position is replaced.
    pub(crate) fn insert(&mut self, block: Block) {
        match self
            .0
            .binary_search_by_key(&block.position, |held| held.position)
        {
            Ok(at) => self.0[at] = block,
            Err(at) => self.0.insert(at, block),
        }
    }

    /// Fails unless the block at `position` links to its adjacent
    /// neighbours: one at or above its parent's position below it, one whose
    /// parent is at or below its position above it. A neighbour further
    /// away is a gap (pruned blocks), which is allowed.
    ///
    /// # Errors
    ///
    /// When it does not link, or a stored value does not decode.
    pub(crate) fn check_links(&self, position: i64) -> anyhow::Result<()> {
        let Some(block) = self.at(position) else {
            return Ok(());
        };
        let network = block.network();
        let header = BlockHeader::try_from(block)?;
        if let Some(below) = self.below(position)
            && block
                .parent_position
                .is_none_or(|parent| below.position >= parent)
            && !below.is_parent_of(&header)
        {
            anyhow::bail!(
                "{network:?}: block {position} does not link onto the kept block below it"
            );
        }
        if let Some(above) = self.above(position)
            && above
                .parent_position
                .is_none_or(|parent| parent <= position)
            && !block.is_parent_of(&BlockHeader::try_from(above)?)
        {
            anyhow::bail!("{network:?}: the kept block above {position} does not link onto it");
        }
        Ok(())
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
        reorged_at: Option<i64>,
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
                reorged_at: row.reorged_at,
                transactions: Vec::new(),
                rewards: Vec::new(),
            }
        }
    }

    /// The live blocks of `network`.
    macro_rules! live {
        ($network:expr) => {
            block::table
                .filter(block::chain.eq($network.chain.as_str()))
                .filter(block::network.eq($network.name.as_str()))
                .filter(block::reorged_at.is_null())
        };
    }

    /// Keeps `block` (see [`Block::try_from`]) live with its facts, in
    /// 072's order:
    ///
    /// 1. its row: a block reorged before is revived, a new one linked to
    ///    its kept parent and its kept child to it;
    /// 2. the wallet and transaction indexes it applies to move onto it, and
    ///    it keeps only the wallet indexes' legs and the transactions found,
    ///    with their legs of the wallet indexes behind it too
    ///    ([`Block::keep`]);
    /// 3. its legs of the wallet indexes its legs are of are deleted (a
    ///    block kept again writes them again);
    /// 4. each such index's seq and totals are read (one query), and each
    ///    leg, in order, takes the next of its index; a spend of a coin the
    ///    index archived (or never saw) subtracts as any other;
    /// 5. each transaction's row is upserted, then every leg inserted.
    ///
    /// # Errors
    ///
    /// When a value does not decode or a write fails (a live block at its
    /// position already, a seq an index holds).
    pub(crate) fn save(connection: &mut SqliteConnection, block: &mut Block) -> anyhow::Result<()> {
        let parent = save_row(connection, block)?;
        let ours = wallet_index::db::advance(connection, block, parent.as_ref())?;
        let watched = transaction_index::db::advance(connection, block, parent.as_ref())?;
        let senders = block.indexes_of(&watched);
        let behind = wallet_index::db::find_behind_block(connection, block, &senders)?;
        block.keep(&ours, &watched, &behind);

        let index_ids: BTreeSet<String> = block
            .legs()
            .map(|leg| leg.wallet_index_id.clone().into())
            .collect();
        leg::db::delete_in(connection, &block.id, block.position, &index_ids)?;

        let mut running = leg::db::find_running(connection, &index_ids, block.position)?;
        let legs = block
            .transactions
            .iter_mut()
            .flat_map(|transaction| &mut transaction.legs)
            .chain(&mut block.rewards);
        for leg in legs {
            let totals = running
                .get_mut(leg.wallet_index_id.as_ref())
                .context("a leg's index was not read")?;
            leg.follow(totals)?;
        }

        for transaction in &block.transactions {
            transaction::db::upsert(connection, transaction)?;
        }
        leg::db::insert(connection, block.legs().collect())?;
        Ok(())
    }

    /// Keeps `block`'s row live, no index moved and none of its facts
    /// written: revives it when kept reorged, else inserts it linked to its
    /// kept parent and links a kept child to it. Its parent's id.
    ///
    /// # Errors
    ///
    /// When a value does not decode or a write fails.
    pub(crate) fn save_row(
        connection: &mut SqliteConnection,
        block: &mut Block,
    ) -> anyhow::Result<Option<BlockId>> {
        let parent = block
            .parent_hash
            .clone()
            .map(|hash| BlockId::new(block.network(), &BlockHash(hash)))
            .transpose()?;
        let revived = diesel::update(block::table.find(&block.id))
            .set(block::reorged_at.eq(None::<i64>))
            .execute(connection)?;
        if revived > 0 {
            return Ok(parent);
        }
        block.parent = match &parent {
            Some(parent) => block::table
                .find(parent)
                .select(block::id)
                .first::<BlockId>(connection)
                .optional()?,
            None => None,
        };
        diesel::insert_into(block::table)
            .values(&*block)
            .execute(connection)?;
        diesel::update(
            block::table
                .filter(block::chain.eq(&block.chain))
                .filter(block::network.eq(&block.network))
                .filter(block::parent.is_null())
                .filter(block::parent_hash.eq(&block.hash)),
        )
        .set(block::parent.eq(&block.id))
        .execute(connection)?;
        Ok(parent)
    }

    /// At most `page.limit` live blocks of `network` below position
    /// `page.after` when given, latest first.
    pub(crate) fn find_page(
        connection: &mut SqliteConnection,
        network: &Network,
        page: Page<i64>,
    ) -> QueryResult<Vec<Block>> {
        let mut query = live!(network).into_boxed();
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

    /// The live block of `network` at `position`.
    pub(crate) fn find_by_position(
        connection: &mut SqliteConnection,
        network: &Network,
        position: i64,
    ) -> QueryResult<Option<Block>> {
        live!(network)
            .filter(block::position.eq(position))
            .select(Row::as_select())
            .first(connection)
            .optional()
            .map(|row| row.map(Block::from))
    }

    /// The live blocks of `network` from the one right below `lowest` to
    /// the one right above `highest`, by position.
    pub(crate) fn find_around(
        connection: &mut SqliteConnection,
        network: &Network,
        lowest: i64,
        highest: i64,
    ) -> QueryResult<Blocks> {
        // The live blocks of a network from the one right below position `?3`
        // to the one right above position `?4`, by position.
        diesel::sql_query(
            "
            SELECT
                b.*
            FROM block b
            WHERE b.chain = ?1
              AND b.network = ?2
              AND b.reorged_at IS NULL
              AND b.position >= COALESCE((
                  SELECT MAX(position)
                  FROM block
                  WHERE chain = ?1
                    AND network = ?2
                    AND reorged_at IS NULL
                    AND position < ?3
              ), ?3)
              AND b.position <= COALESCE((
                  SELECT MIN(position)
                  FROM block
                  WHERE chain = ?1
                    AND network = ?2
                    AND reorged_at IS NULL
                    AND position > ?4
              ), ?4)
            ORDER BY b.position",
        )
        .bind::<Text, _>(network.chain.as_str())
        .bind::<Text, _>(&network.name)
        .bind::<BigInt, _>(lowest)
        .bind::<BigInt, _>(highest)
        .load::<Row>(connection)
        .map(|rows| rows.into_iter().map(Block::from).collect())
    }

    /// How many blocks of every network are kept live, and how many a reorg
    /// took back.
    pub(crate) fn count(connection: &mut SqliteConnection) -> QueryResult<(i64, i64)> {
        block::table
            .select(diesel::dsl::sql::<(BigInt, BigInt)>(
                "COUNT(*) - COUNT(reorged_at), COUNT(reorged_at)",
            ))
            .get_result(connection)
    }

    /// The stamp (Unix seconds) of the newest live block of any network.
    pub(crate) fn find_newest_stamp(connection: &mut SqliteConnection) -> QueryResult<Option<i64>> {
        block::table
            .filter(block::reorged_at.is_null())
            .select(diesel::dsl::max(block::timestamp))
            .first(connection)
    }

    /// How many live blocks of every network are stamped in each hour from
    /// `since` (Unix seconds) on, as (hour, count); hour 0 starts at
    /// `since`.
    pub(crate) fn count_by_hour(
        connection: &mut SqliteConnection,
        since: i64,
    ) -> QueryResult<Vec<(i64, i64)>> {
        block::table
            .filter(block::reorged_at.is_null())
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

    /// The live block of `network` right below `position`.
    pub(crate) fn find_below(
        connection: &mut SqliteConnection,
        network: &Network,
        position: i64,
    ) -> QueryResult<Option<Block>> {
        live!(network)
            .filter(block::position.lt(position))
            .order(block::position.desc())
            .select(Row::as_select())
            .first(connection)
            .optional()
            .map(|row| row.map(Block::from))
    }

    /// Marks every live block of `network` at or above `from` reorged at
    /// `now` (Unix ms).
    pub(crate) fn reorg(
        connection: &mut SqliteConnection,
        network: &Network,
        from: i64,
        now: i64,
    ) -> QueryResult<usize> {
        diesel::update(live!(network).filter(block::position.ge(from)))
            .set(block::reorged_at.eq(now))
            .execute(connection)
    }

    /// Whether an index names block `block`: a wallet or transaction index
    /// indexed through it, or a transaction index that found its
    /// transaction in it. Such a block stays (its foreign keys do not
    /// cascade).
    const NAMED: &str = "
        EXISTS (
            SELECT 1 FROM wallet_index WHERE indexed_at = block.id
            UNION ALL
            SELECT 1 FROM transaction_index WHERE indexed_at = block.id OR found = block.id
        )";

    /// Deletes the blocks of `network` below position `below` nothing needs
    /// ([`delete_reorged`], [`delete_unused`]). How many went.
    pub(crate) fn prune(
        connection: &mut SqliteConnection,
        network: &Network,
        below: i64,
    ) -> QueryResult<usize> {
        let reorged = delete_reorged(connection, network, below)?;
        Ok(reorged + delete_unused(connection, network, below)?)
    }

    /// Deletes the reorged blocks of `network` below position `below` no
    /// index names: the chain took them back, so what was on them (legs,
    /// transactions) goes with them by cascade, and a kept child's `parent`
    /// is set NULL. How many.
    pub(crate) fn delete_reorged(
        connection: &mut SqliteConnection,
        network: &Network,
        below: i64,
    ) -> QueryResult<usize> {
        diesel::sql_query(format!(
            "
            DELETE FROM block
            WHERE chain = ?1
              AND network = ?2
              AND position < ?3
              AND reorged_at IS NOT NULL
              AND NOT {NAMED}"
        ))
        .bind::<Text, _>(network.chain.as_str())
        .bind::<Text, _>(&network.name)
        .bind::<BigInt, _>(below)
        .execute(connection)
    }

    /// Deletes the live blocks of `network` below position `below` that
    /// hold nothing: no leg, no transaction, no index names them; the
    /// newest live block always stays (the indexer checks it is still on
    /// the chain). Never a block with history: nothing cascades. How many.
    pub(crate) fn delete_unused(
        connection: &mut SqliteConnection,
        network: &Network,
        below: i64,
    ) -> QueryResult<usize> {
        diesel::sql_query(format!(
            r#"
            DELETE FROM block
            WHERE chain = ?1
              AND network = ?2
              AND position < ?3
              AND reorged_at IS NULL
              AND position < (
                  SELECT MAX(newest.position)
                  FROM block AS newest
                  WHERE newest.chain = ?1
                    AND newest.network = ?2
                    AND newest.reorged_at IS NULL
              )
              AND NOT EXISTS (SELECT 1 FROM leg WHERE leg.block = block.id)
              AND NOT EXISTS (SELECT 1 FROM "transaction" AS t WHERE t.block = block.id)
              AND NOT {NAMED}"#
        ))
        .bind::<Text, _>(network.chain.as_str())
        .bind::<Text, _>(&network.name)
        .bind::<BigInt, _>(below)
        .execute(connection)
    }
}
