//! `block`: every block kept, live or archived, of every network.

use base::{BlockHash, BlockHeader, BlockParent, Network};
use diesel::{prelude::*, sqlite::Sqlite};

use super::{int, key, schema::block, uint};

/// One `block` row.
#[derive(Clone, Debug, Queryable, Selectable)]
#[diesel(table_name = block, check_for_backend(Sqlite))]
pub(crate) struct Block {
    pub(crate) id: String,
    #[allow(
        dead_code,
        reason = "the whole row is selected; the network is the query's"
    )]
    pub(crate) chain: String,
    #[allow(
        dead_code,
        reason = "the whole row is selected; the network is the query's"
    )]
    pub(crate) network: String,
    pub(crate) position: i64,
    pub(crate) height: i64,
    pub(crate) hash: Vec<u8>,
    #[allow(
        dead_code,
        reason = "the whole row is selected; the link is the schema's"
    )]
    pub(crate) parent: Option<String>,
    pub(crate) parent_position: Option<i64>,
    pub(crate) parent_hash: Option<Vec<u8>>,
    pub(crate) timestamp: Option<i64>,
    #[allow(
        dead_code,
        reason = "the whole row is selected; reads filter on it in SQL"
    )]
    pub(crate) archived_at: Option<i64>,
}

impl Block {
    /// The header it was written from.
    ///
    /// # Errors
    ///
    /// When a stored integer is negative.
    pub(crate) fn header(&self) -> anyhow::Result<BlockHeader> {
        let parent = match (self.parent_position, &self.parent_hash) {
            (Some(position), Some(hash)) => Some(BlockParent {
                position: uint(position)?,
                hash: BlockHash(hash.clone()),
            }),
            _ => None,
        };
        Ok(BlockHeader {
            position: uint(self.position)?,
            height: uint(self.height)?,
            hash: BlockHash(self.hash.clone()),
            parent,
            timestamp: self.timestamp.map(uint).transpose()?,
        })
    }

    /// Whether this row is `header`'s block.
    pub(crate) fn is(&self, header: &BlockHeader) -> bool {
        self.header().is_ok_and(|own| own == *header)
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
}

/// The `block` table's statements.
pub(crate) mod db {
    #[allow(clippy::wildcard_imports, reason = "the model file's own items")]
    use super::*;

    #[derive(Insertable)]
    #[diesel(table_name = block)]
    struct NewBlock<'a> {
        id: &'a str,
        chain: &'a str,
        network: &'a str,
        position: i64,
        height: i64,
        hash: &'a [u8],
        parent: Option<&'a str>,
        parent_position: Option<i64>,
        parent_hash: Option<&'a [u8]>,
        timestamp: Option<i64>,
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

    /// Adds `header` as a live block of `network`, linked to its parent when
    /// that is stored, and links a stored child to it; a block archived
    /// before is revived. Its id.
    ///
    /// # Errors
    ///
    /// When a value is out of range or the write fails (a live block at its
    /// position already).
    pub(crate) fn insert(
        connection: &mut SqliteConnection,
        network: &Network,
        header: &BlockHeader,
    ) -> anyhow::Result<String> {
        let id = Block::id(network, &header.hash)?;
        let revived = diesel::update(block::table.find(&id))
            .set(block::archived_at.eq(None::<i64>))
            .execute(connection)?;
        if revived > 0 {
            return Ok(id);
        }
        let parent = match &header.parent {
            Some(parent) => {
                let parent = Block::id(network, &parent.hash)?;
                block::table
                    .find(&parent)
                    .select(block::id)
                    .first::<String>(connection)
                    .optional()?
            }
            None => None,
        };
        let row = NewBlock {
            id: &id,
            chain: network.chain.as_str(),
            network: &network.name,
            position: int(header.position)?,
            height: int(header.height)?,
            hash: &header.hash.0,
            parent: parent.as_deref(),
            parent_position: header
                .parent
                .as_ref()
                .map(|p| int(p.position))
                .transpose()?,
            parent_hash: header.parent.as_ref().map(|p| p.hash.0.as_slice()),
            timestamp: header.timestamp.map(int).transpose()?,
        };
        diesel::insert_into(block::table)
            .values(&row)
            .execute(connection)?;
        diesel::update(
            block::table
                .filter(block::chain.eq(network.chain.as_str()))
                .filter(block::network.eq(&network.name))
                .filter(block::parent.is_null())
                .filter(block::parent_hash.eq(&header.hash.0)),
        )
        .set(block::parent.eq(&id))
        .execute(connection)?;
        Ok(id)
    }

    /// The block of row `id`.
    pub(crate) fn get_by_id(connection: &mut SqliteConnection, id: &str) -> QueryResult<Block> {
        block::table
            .find(id)
            .select(Block::as_select())
            .first(connection)
    }

    /// The live block of `network` at `position`.
    pub(crate) fn find_live(
        connection: &mut SqliteConnection,
        network: &Network,
        position: i64,
    ) -> QueryResult<Option<Block>> {
        live!(network)
            .filter(block::position.eq(position))
            .select(Block::as_select())
            .first(connection)
            .optional()
    }

    /// The last live block of `network`.
    pub(crate) fn find_last(
        connection: &mut SqliteConnection,
        network: &Network,
    ) -> QueryResult<Option<Block>> {
        live!(network)
            .order(block::position.desc())
            .select(Block::as_select())
            .first(connection)
            .optional()
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
            .select(Block::as_select())
            .first(connection)
            .optional()
    }

    /// The live block of `network` right above `position`.
    pub(crate) fn find_above(
        connection: &mut SqliteConnection,
        network: &Network,
        position: i64,
    ) -> QueryResult<Option<Block>> {
        live!(network)
            .filter(block::position.gt(position))
            .order(block::position.asc())
            .select(Block::as_select())
            .first(connection)
            .optional()
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
