//! `tag`: the labels on an address of a network, which only select its
//! wallet indexes (pipes).

use anyhow::ensure;
use base::Address;
use diesel::{prelude::*, sqlite::Sqlite};

use super::{key, schema::tag};

/// One tag on an address.
#[derive(Clone, Debug, Queryable, Selectable, Insertable)]
#[diesel(table_name = tag, check_for_backend(Sqlite))]
pub(crate) struct Tag {
    pub(crate) id: String,
    pub(crate) chain: String,
    pub(crate) network: String,
    pub(crate) address: String,
    #[diesel(column_name = name)]
    pub(crate) tag: String,
}

impl Tag {
    /// Tag `tag` on `address`.
    ///
    /// # Errors
    ///
    /// When `tag` is not `[a-z0-9-]{1,64}`, or a part holds a dot.
    pub(crate) fn new(address: &Address, tag: &str) -> anyhow::Result<Self> {
        Ok(Self {
            id: Self::id(address, tag)?,
            chain: address.network.chain.as_str().to_owned(),
            network: address.network.name.clone(),
            address: address.value.clone(),
            tag: tag.to_owned(),
        })
    }

    /// The id of tag `tag` on `address`: `"{chain}.{network}.{address}.{tag}"`.
    ///
    /// # Errors
    ///
    /// When `tag` is not `[a-z0-9-]{1,64}`, or a part holds a dot.
    pub(crate) fn id(address: &Address, tag: &str) -> anyhow::Result<String> {
        ensure!(
            (1..=64).contains(&tag.len())
                && tag
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "a tag is 1 to 64 of [a-z0-9-]: {tag:?}"
        );
        key(&[
            address.network.chain.as_str(),
            &address.network.name,
            &address.value,
            tag,
        ])
    }
}

collection! {
    /// Tags, ordered by name.
    Tags(Tag)
}

impl Tags {
    /// Their names, in order.
    pub(crate) fn names(&self) -> Vec<&str> {
        self.iter().map(|tag| tag.tag.as_str()).collect()
    }
}

/// The `tag` table's statements.
#[allow(
    dead_code,
    reason = "the table's whole API; not every statement has a caller yet"
)]
pub(crate) mod db {
    use base::Address;
    use diesel::prelude::*;

    use super::{Tag, Tags};
    use crate::models::schema::tag;

    /// How many tags every network keeps.
    pub(crate) fn count(connection: &mut SqliteConnection) -> QueryResult<i64> {
        tag::table.count().get_result(connection)
    }

    /// Keeps `tag`; a kept tag stays once.
    pub(crate) fn insert(connection: &mut SqliteConnection, tag: &Tag) -> QueryResult<usize> {
        diesel::insert_or_ignore_into(tag::table)
            .values(tag)
            .execute(connection)
    }

    /// Removes tag `id`; how many went (0 or 1).
    pub(crate) fn delete(connection: &mut SqliteConnection, id: &str) -> QueryResult<usize> {
        diesel::delete(tag::table.find(id)).execute(connection)
    }

    /// The tags of `address`, by name.
    pub(crate) fn find_by_address(
        connection: &mut SqliteConnection,
        address: &Address,
    ) -> QueryResult<Tags> {
        let tags = tag::table
            .filter(tag::chain.eq(address.network.chain.as_str()))
            .filter(tag::network.eq(&address.network.name))
            .filter(tag::address.eq(&address.value))
            .order(tag::name)
            .select(Tag::as_select())
            .load(connection)?;
        Ok(tags.into())
    }

    /// Every tag, by network, address, then name.
    pub(crate) fn find_all(connection: &mut SqliteConnection) -> QueryResult<Tags> {
        let tags = tag::table
            .order((tag::chain, tag::network, tag::address, tag::name))
            .select(Tag::as_select())
            .load(connection)?;
        Ok(tags.into())
    }
}
