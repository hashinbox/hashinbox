//! `block`: every block kept, live or archived, of every network.

use std::collections::BTreeSet;

use anyhow::Context as _;
use base::{BlockHash, BlockHeader, BlockParent, Network};
use diesel::{
    prelude::*,
    sql_types::{BigInt, Text},
    sqlite::Sqlite,
};

use super::{
    Json, Leg, int, key, leg, schema::block, transaction, transaction::Transaction, uint,
    wallet_index,
};

/// Serializes bytes as lowercase hexadecimal.
fn hex_text<S: serde::Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&hex::encode(bytes))
}

/// A kept block: its `block` row, its kept transactions in block order and
/// its kept rewards. The API shows it as is. Read whole in one query: its
/// children come as JSON arrays SQLite builds (`transactions`, `rewards`).
#[derive(Clone, Debug, Insertable, QueryableByName, serde::Serialize, utoipa::ToSchema)]
#[diesel(table_name = block, check_for_backend(Sqlite))]
pub(crate) struct Block {
    /// `{chain}.{network}.{hash}`.
    pub(crate) id: String,
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
    pub(crate) parent: Option<String>,
    /// Its parent's position; null for genesis.
    pub(crate) parent_position: Option<i64>,
    #[serde(skip)]
    pub(crate) parent_hash: Option<Vec<u8>>,
    /// Unix seconds, when reported.
    pub(crate) timestamp: Option<i64>,
    /// Unix ms a reorg archived it; null while live.
    pub(crate) archived_at: Option<i64>,
    /// Its kept transactions, in block order.
    #[diesel(skip_insertion, sql_type = Text, deserialize_as = Json<Vec<Transaction>>)]
    pub(crate) transactions: Vec<Transaction>,
    /// Its kept rewards, in order.
    #[diesel(skip_insertion, sql_type = Text, deserialize_as = Json<Vec<Leg>>)]
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
        let id = Self::id(&block.network, &header.hash)?;
        let position = int(header.position)?;
        let mut transactions = block
            .transactions
            .iter()
            .map(Transaction::try_from)
            .collect::<anyhow::Result<Vec<_>>>()?;
        transactions.sort_by_key(|transaction| transaction.order.unwrap_or(0));
        for transaction in &mut transactions {
            transaction.block = Some(id.clone());
            transaction.expires_at = None;
            for leg in &mut transaction.legs {
                leg.block = Some(id.clone());
                leg.position = Some(position);
            }
        }
        let mut rewards = block
            .rewards
            .iter()
            .map(|reward| Leg::try_from((id.as_str(), reward)))
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
            archived_at: None,
            transactions,
            rewards,
            id,
        })
    }
}

impl Block {
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

    /// The id of `network`'s block `hash`: `"{chain}.{network}.{hex}"`.
    ///
    /// # Errors
    ///
    /// When a part holds a dot.
    pub(crate) fn id(network: &Network, hash: &BlockHash) -> anyhow::Result<String> {
        key(&[network.chain.as_str(), &network.name, &hex::encode(&hash.0)])
    }

    /// Drops its legs of wallet indexes not in `ours` (index ids), then its
    /// transactions left without a leg.
    pub(crate) fn keep(&mut self, ours: &BTreeSet<String>) {
        for transaction in &mut self.transactions {
            transaction.keep(ours);
        }
        self.transactions
            .retain(|transaction| !transaction.legs.is_empty());
        self.rewards
            .retain(|reward| ours.contains(&reward.wallet_index_id));
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

/// The `block` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    /// One `block` row.
    #[derive(Queryable, Selectable)]
    #[diesel(table_name = block, check_for_backend(Sqlite))]
    pub(in crate::models) struct Row {
        id: String,
        chain: String,
        network: String,
        position: i64,
        height: i64,
        hash: Vec<u8>,
        parent: Option<String>,
        parent_position: Option<i64>,
        parent_hash: Option<Vec<u8>>,
        timestamp: Option<i64>,
        archived_at: Option<i64>,
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
                archived_at: row.archived_at,
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
                .filter(block::archived_at.is_null())
        };
    }

    /// Keeps `block` (see [`Block::try_from`]) live with its facts, in
    /// 072's order:
    ///
    /// 1. its row: a block archived before is revived, a new one linked to
    ///    its kept parent and its kept child to it;
    /// 2. the wallet indexes it applies to move onto it, and it keeps only
    ///    their legs ([`Block::keep`]);
    /// 3. its legs of the wallet indexes its legs are of are deleted (a
    ///    block kept again writes them again);
    /// 4. each such index's seq and total are read (one query), and each
    ///    leg, in order, takes the next of its index;
    /// 5. each transaction's row is upserted, then every leg inserted.
    ///
    /// # Errors
    ///
    /// When a value does not decode or a write fails (a live block at its
    /// position already, a seq an index holds).
    pub(crate) fn save(connection: &mut SqliteConnection, block: &mut Block) -> anyhow::Result<()> {
        let parent = block
            .parent_hash
            .as_ref()
            .map(|hash| key(&[&block.chain, &block.network, &hex::encode(hash)]))
            .transpose()?;
        save_row(connection, block, parent.as_deref())?;
        let ours = wallet_index::db::advance(connection, block, parent.as_deref())?;
        block.keep(&ours);

        let index_ids: BTreeSet<String> = block
            .legs()
            .map(|leg| leg.wallet_index_id.clone())
            .collect();
        leg::db::delete_in(connection, &block.id, &index_ids)?;

        let mut running = leg::db::find_running(connection, &index_ids, block.position)?;
        let legs = block
            .transactions
            .iter_mut()
            .flat_map(|transaction| &mut transaction.legs)
            .chain(&mut block.rewards);
        for leg in legs {
            let (seq, total) = running
                .get_mut(&leg.wallet_index_id)
                .context("a leg's index was not read")?;
            *seq += 1;
            *total = total.clone() + leg.signed()?;
            leg.seq = Some(*seq);
            leg.total = Some(total.to_string());
        }

        for transaction in &block.transactions {
            transaction::db::upsert(connection, transaction)?;
        }
        leg::db::insert(connection, block.legs().collect())?;
        Ok(())
    }

    /// Keeps `block`'s row live: revives it when kept archived, else
    /// inserts it linked to its kept parent (id `parent`) and links a kept
    /// child to it.
    fn save_row(
        connection: &mut SqliteConnection,
        block: &mut Block,
        parent: Option<&str>,
    ) -> anyhow::Result<()> {
        let revived = diesel::update(block::table.find(&block.id))
            .set(block::archived_at.eq(None::<i64>))
            .execute(connection)?;
        if revived > 0 {
            return Ok(());
        }
        block.parent = match parent {
            Some(parent) => block::table
                .find(parent)
                .select(block::id)
                .first::<String>(connection)
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
        Ok(())
    }

    /// The `limit` last live blocks of `network`, latest first, with their
    /// transactions, legs and rewards.
    pub(crate) fn find_latest(
        connection: &mut SqliteConnection,
        network: &Network,
        limit: i64,
    ) -> QueryResult<Vec<Block>> {
        // The last `?3` live blocks of a network, latest first, each with its
        // transactions (each with its legs) and rewards.
        diesel::sql_query(
            r#"
            SELECT
                b.*,
                COALESCE((
                    SELECT json_group_array(
                        json_object(
                            'id', t.id,
                            'block', t.block,
                            'chain', t.chain,
                            'network', t.network,
                            'txid', t.txid,
                            'order', t."order",
                            'state', t.state,
                            'reason', t.reason,
                            'sent_at', t.sent_at,
                            'expires_at', t.expires_at,
                            'legs', json(COALESCE((
                                SELECT json_group_array(
                                    json(l.json)
                                    ORDER BY l.leg_index
                                )
                                FROM leg_json l
                                WHERE l."transaction" = t.id
                            ), '[]'))
                        )
                        ORDER BY t."order"
                    )
                    FROM "transaction" t
                    WHERE t.block = b.id
                ), '[]') AS transactions,
                COALESCE((
                    SELECT json_group_array(
                        json(l.json)
                        ORDER BY l.leg_index
                    )
                    FROM leg_json l
                    WHERE l."transaction" IS NULL
                      AND l.block = b.id
                ), '[]') AS rewards
            FROM block b
            WHERE b.chain = ?1
              AND b.network = ?2
              AND b.archived_at IS NULL
            ORDER BY b.position DESC
            LIMIT ?3"#,
        )
        .bind::<Text, _>(network.chain.as_str())
        .bind::<Text, _>(&network.name)
        .bind::<BigInt, _>(limit)
        .load(connection)
    }

    /// The live block of `network` at `position`, with its transactions,
    /// legs and rewards.
    pub(crate) fn find_at(
        connection: &mut SqliteConnection,
        network: &Network,
        position: i64,
    ) -> QueryResult<Option<Block>> {
        // The live block of a network at position `?3`, with its transactions
        // (each with its legs) and rewards.
        diesel::sql_query(
            r#"
            SELECT
                b.*,
                COALESCE((
                    SELECT json_group_array(
                        json_object(
                            'id', t.id,
                            'block', t.block,
                            'chain', t.chain,
                            'network', t.network,
                            'txid', t.txid,
                            'order', t."order",
                            'state', t.state,
                            'reason', t.reason,
                            'sent_at', t.sent_at,
                            'expires_at', t.expires_at,
                            'legs', json(COALESCE((
                                SELECT json_group_array(
                                    json(l.json)
                                    ORDER BY l.leg_index
                                )
                                FROM leg_json l
                                WHERE l."transaction" = t.id
                            ), '[]'))
                        )
                        ORDER BY t."order"
                    )
                    FROM "transaction" t
                    WHERE t.block = b.id
                ), '[]') AS transactions,
                COALESCE((
                    SELECT json_group_array(
                        json(l.json)
                        ORDER BY l.leg_index
                    )
                    FROM leg_json l
                    WHERE l."transaction" IS NULL
                      AND l.block = b.id
                ), '[]') AS rewards
            FROM block b
            WHERE b.chain = ?1
              AND b.network = ?2
              AND b.archived_at IS NULL
              AND b.position = ?3"#,
        )
        .bind::<Text, _>(network.chain.as_str())
        .bind::<Text, _>(&network.name)
        .bind::<BigInt, _>(position)
        .get_result(connection)
        .optional()
    }

    /// The live blocks of `network` from the one right below `lowest` to
    /// the one right above `highest`, by position.
    pub(crate) fn find_around(
        connection: &mut SqliteConnection,
        network: &Network,
        lowest: i64,
        highest: i64,
    ) -> QueryResult<Vec<Block>> {
        // The live blocks of a network from the one right below position `?3`
        // to the one right above position `?4`, by position; their
        // transactions and rewards not read.
        diesel::sql_query(
            "
            SELECT
                b.*,
                '[]' AS transactions,
                '[]' AS rewards
            FROM block b
            WHERE b.chain = ?1
              AND b.network = ?2
              AND b.archived_at IS NULL
              AND b.position >= COALESCE((
                  SELECT MAX(position)
                  FROM block
                  WHERE chain = ?1
                    AND network = ?2
                    AND archived_at IS NULL
                    AND position < ?3
              ), ?3)
              AND b.position <= COALESCE((
                  SELECT MIN(position)
                  FROM block
                  WHERE chain = ?1
                    AND network = ?2
                    AND archived_at IS NULL
                    AND position > ?4
              ), ?4)
            ORDER BY b.position",
        )
        .bind::<Text, _>(network.chain.as_str())
        .bind::<Text, _>(&network.name)
        .bind::<BigInt, _>(lowest)
        .bind::<BigInt, _>(highest)
        .load(connection)
    }

    /// The last live block of `network`.
    pub(crate) fn find_last(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> QueryResult<Option<Block>> {
        live!(network)
            .order(block::position.desc())
            .select(Row::as_select())
            .first(connection)
            .optional()
            .map(|row| row.map(Block::from))
    }

    /// How many blocks of every network are kept live, and how many a reorg
    /// archived.
    pub(crate) fn count(connection: &mut SqliteConnection) -> QueryResult<(i64, i64)> {
        block::table
            .select(diesel::dsl::sql::<(BigInt, BigInt)>(
                "COUNT(*) - COUNT(archived_at), COUNT(archived_at)",
            ))
            .get_result(connection)
    }

    /// The stamp (Unix seconds) of the newest live block of any network.
    pub(crate) fn find_newest_stamp(connection: &mut SqliteConnection) -> QueryResult<Option<i64>> {
        block::table
            .filter(block::archived_at.is_null())
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
            .filter(block::archived_at.is_null())
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

    /// Archives, at `now` (Unix ms), every live block of `network` at or
    /// above `from`.
    pub(crate) fn archive(
        connection: &mut SqliteConnection,
        network: &Network,
        from: i64,
        now: i64,
    ) -> QueryResult<usize> {
        diesel::update(live!(network).filter(block::position.ge(from)))
            .set(block::archived_at.eq(now))
            .execute(connection)
    }
}
