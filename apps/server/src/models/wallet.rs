//! `wallet`: every address the server knows on a network, keyed (its key's
//! curve and bytes kept) or watch-only, its tags, and (read at a tip) its
//! indexes with their balances, progress and legs.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::bail;
use base::{Address, Addressed, Chain, Network, Networked};
use crypto::{Curve, SecretBytes};
use diesel::{deserialize::FromSqlRow, expression::AsExpression};
use serde::Serialize;

use super::{Id, IdError, Json, id::typed_id, wallet_index::WalletIndex};

/// A wallet's id: `{chain}.{network}.{address}`.
#[derive(Clone, Debug, PartialEq, Eq, AsExpression, FromSqlRow)]
#[diesel(sql_type = diesel::sql_types::Text)]
pub(crate) struct WalletId {
    /// Its network and address text, as the id spells it.
    pub(crate) address: Address,
    id: Id,
}

typed_id!(WalletId, address.network);

impl Addressed for WalletId {
    fn address(&self) -> &Address {
        &self.address
    }
}

impl WalletId {
    /// The id of the wallet of `address`.
    ///
    /// # Errors
    ///
    /// When a part holds a dot.
    pub(crate) fn of(address: &Address) -> Result<Self, IdError> {
        let Address { network, value } = address;
        let id = Id::new(&[network.chain.as_str(), &network.name, value])?;
        Ok(Self {
            address: address.clone(),
            id,
        })
    }

    /// `text` as a wallet id; `Container::parse` makes its address canonical.
    ///
    /// # Errors
    ///
    /// Unless it has its three parts.
    pub(crate) fn parse(text: &str) -> Result<Self, IdError> {
        let [chain, network, address] = Id::parts::<3>(text, "{chain}.{network}.{address}")?;
        Self::of(&Address::new(
            Network::new(Chain::new(chain), network),
            address,
        ))
    }

    /// The row id.
    #[must_use]
    pub(crate) fn id(&self) -> &Id {
        &self.id
    }
}

/// Key bytes, zeroized on drop; `None` for a watch-only wallet.
pub(crate) struct Secret(pub(crate) Option<SecretBytes>);

impl From<Option<Vec<u8>>> for Secret {
    fn from(bytes: Option<Vec<u8>>) -> Self {
        Self(bytes.map(SecretBytes::new))
    }
}

/// One wallet, as the API shows it: an address, whether its key is kept,
/// its tags, and its indexes (each with its balances, progress and legs;
/// read by [`db::find`] and [`db::find_page`], empty from the other
/// reads). The key (`curve`, `secret`) is never serialized, and the API
/// reads leave it out. No `Debug`: it may hold the key.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct Wallet {
    /// `{chain}.{network}.{address}`: what every wallet route takes.
    #[schema(value_type = String)]
    pub(crate) id: WalletId,
    pub(crate) chain: String,
    pub(crate) network: String,
    /// Canonical address text.
    pub(crate) address: String,
    /// Whether this server holds its key and can send from it.
    pub(crate) keyed: bool,
    /// Its tags, sorted, each once, each [`is_tag`].
    #[schema(value_type = Vec<String>)]
    pub(crate) tags: Json<BTreeSet<String>>,
    /// The caller's own reference for it (e.g. their customer id), 1 to 128
    /// characters; not unique. Null when none was given.
    #[schema(required)]
    pub(crate) external_id: Option<String>,
    /// Its indexes by asset id (`native` or the token id).
    pub(crate) indexes: BTreeMap<String, WalletIndex>,
    /// `None` exactly when `secret` is.
    #[serde(skip)]
    pub(crate) curve: Option<String>,
    #[serde(skip)]
    pub(crate) secret: Secret,
}

/// Whether `tag` is a tag: 1 to 64 of `[a-z0-9-]`.
pub(crate) fn is_tag(tag: &str) -> bool {
    (1..=64).contains(&tag.len())
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl Wallet {
    /// The wallet of `address` with its key, untagged.
    ///
    /// # Errors
    ///
    /// When a part of the address holds a dot.
    pub(crate) fn new(
        address: &Address,
        curve: Curve,
        secret: SecretBytes,
    ) -> anyhow::Result<Self> {
        let curve = match curve {
            Curve::Secp256k1 => "secp256k1",
            Curve::Ed25519 => "ed25519",
        };
        Ok(Self {
            keyed: true,
            curve: Some(curve.to_owned()),
            secret: Secret(Some(secret)),
            ..Self::from_address(address)?
        })
    }

    /// The watch-only wallet of `address`, untagged.
    ///
    /// # Errors
    ///
    /// When a part of the address holds a dot.
    pub(crate) fn from_address(address: &Address) -> anyhow::Result<Self> {
        Ok(Self {
            id: WalletId::of(address)?,
            chain: address.network.chain.as_str().to_owned(),
            network: address.network.name.clone(),
            address: address.value.clone(),
            keyed: false,
            tags: Json(BTreeSet::new()),
            external_id: None,
            indexes: BTreeMap::new(),
            curve: None,
            secret: Secret(None),
        })
    }

    /// Its key's curve; `None` for a watch-only wallet.
    ///
    /// # Errors
    ///
    /// When the stored name is unknown.
    pub(crate) fn curve(&self) -> anyhow::Result<Option<Curve>> {
        Ok(match self.curve.as_deref() {
            None => None,
            Some("secp256k1") => Some(Curve::Secp256k1),
            Some("ed25519") => Some(Curve::Ed25519),
            Some(_) => bail!("a stored wallet curve is unknown"),
        })
    }
}

/// A wallet with its key, as kept: its address, curve and key bytes.
impl TryFrom<&dyn base::Wallet> for Wallet {
    type Error = anyhow::Error;

    fn try_from(wallet: &dyn base::Wallet) -> anyhow::Result<Self> {
        let (curve, secret) = wallet.private_key();
        Self::new(wallet.address(), curve, secret)
    }
}

collection! {
    /// Wallets, ordered by id.
    Wallets(Wallet)
}

impl Networked for Wallet {
    fn network(&self) -> &Network {
        self.id.network()
    }
}

impl Addressed for Wallet {
    fn address(&self) -> &Address {
        self.id.address()
    }
}

/// Its id.
impl crate::db::Keyed for Wallet {
    fn key(&self) -> String {
        self.id.clone().into()
    }
}

/// The `wallet` table's statements.
#[allow(
    dead_code,
    reason = "the table's whole API; not every statement has a caller yet"
)]
pub(crate) mod db {
    use std::collections::{BTreeMap, BTreeSet};

    use anyhow::ensure;
    use diesel::{
        dsl::sql,
        prelude::*,
        sql_types::{BigInt, Binary, Bool, Nullable, Text},
        sqlite::Sqlite,
    };

    use super::{Json, Secret, Wallet, WalletId, Wallets, is_tag};
    use crate::{
        db::Page,
        models::{
            schema::wallet,
            wallet_index::{WalletIndex, db::IndexRow},
        },
    };

    /// One `wallet` row: a [`Wallet`] with its key, without its indexes.
    #[derive(Queryable, QueryableByName, Selectable)]
    #[diesel(table_name = wallet, check_for_backend(Sqlite))]
    struct Row {
        id: WalletId,
        chain: String,
        network: String,
        address: String,
        curve: Option<String>,
        #[diesel(deserialize_as = Option<Vec<u8>>)]
        secret: Secret,
        tags: Json<BTreeSet<String>>,
        external_id: Option<String>,
    }

    impl From<Row> for Wallet {
        fn from(row: Row) -> Self {
            Self {
                id: row.id,
                chain: row.chain,
                network: row.network,
                address: row.address,
                keyed: row.secret.0.is_some(),
                tags: row.tags,
                external_id: row.external_id,
                indexes: BTreeMap::new(),
                curve: row.curve,
                secret: row.secret,
            }
        }
    }

    /// A row of [`find`] / [`find_page`]: a wallet with its indexes.
    #[derive(QueryableByName)]
    struct AtTip {
        #[diesel(sql_type = Text)]
        id: WalletId,
        #[diesel(sql_type = Text)]
        chain: String,
        #[diesel(sql_type = Text)]
        network: String,
        #[diesel(sql_type = Text)]
        address: String,
        #[diesel(sql_type = Bool)]
        keyed: bool,
        #[diesel(sql_type = Text)]
        tags: Json<BTreeSet<String>>,
        #[diesel(sql_type = Nullable<Text>)]
        external_id: Option<String>,
        #[diesel(sql_type = Text)]
        indexes: Json<BTreeMap<String, IndexRow>>,
    }

    /// The row as its wallet, without its key.
    impl TryFrom<AtTip> for Wallet {
        type Error = anyhow::Error;

        fn try_from(row: AtTip) -> anyhow::Result<Self> {
            let indexes = row
                .indexes
                .0
                .into_iter()
                .map(|(asset, index)| Ok((asset, WalletIndex::try_from(index)?)))
                .collect::<anyhow::Result<_>>()?;
            Ok(Self {
                id: row.id,
                chain: row.chain,
                network: row.network,
                address: row.address,
                keyed: row.keyed,
                tags: row.tags,
                external_id: row.external_id,
                indexes,
                curve: None,
                secret: Secret(None),
            })
        }
    }

    /// Wallet `id`, if kept, with its indexes measured against their
    /// network's `network` row, in one query. Never its key.
    ///
    /// # Errors
    ///
    /// When the read fails or a stored value does not decode.
    pub(crate) fn find(
        connection: &mut SqliteConnection,
        id: impl AsRef<str>,
    ) -> anyhow::Result<Option<Wallet>> {
        let id = id.as_ref();
        // Wallet `?1` with its indexes (as `wallet_index_with_balance` reads
        // them) as one JSON object by asset. No key is read.
        diesel::sql_query(
            r#"
            SELECT
                w.id,
                w.chain,
                w.network,
                w.address,
                w.secret IS NOT NULL AS keyed,
                w.tags,
                w.external_id,
                COALESCE((
                    SELECT json_group_object(
                        wi.asset,
                        json_object(
                            'id', wi.id,
                            'wallet_id', wi.wallet_id,
                            'asset', wi.asset,
                            'requested_from', wi.requested_from,
                            'birthday', wi.birthday,
                            'indexed_at', wi.indexed_at,
                            'state', wi.state,
                            'reason', wi.reason,
                            'indexed_through', wi.indexed_through,
                            'behind_tip', wi.behind_tip,
                            'finality_at', wi.finality_at,
                            'balance_confirmed', wi.balance_confirmed,
                            'balance_confirmed_received', wi.balance_confirmed_received,
                            'balance_confirmed_sent', wi.balance_confirmed_sent,
                            'balance_pending', wi.balance_pending,
                            'keyed', json(iif(wi.keyed, 'true', 'false')),
                            'balance_confirmed_spendable', wi.balance_confirmed_spendable
                        )
                    )
                    FROM wallet_index_with_balance wi
                    WHERE wi.wallet_id = w.id
                ), '{}') AS indexes
            FROM wallet w
            WHERE w.id = ?1"#,
        )
        .bind::<Text, _>(id)
        .get_result::<AtTip>(connection)
        .optional()?
        .map(Wallet::try_from)
        .transpose()
    }

    /// At most `page.limit` wallets with an id after `page.after` when
    /// given, by id, each as [`find`], in one query.
    ///
    /// # Errors
    ///
    /// As [`find`].
    pub(crate) fn find_page(
        connection: &mut SqliteConnection,
        page: Page<String>,
    ) -> anyhow::Result<Vec<Wallet>> {
        // As in [`find`], for at most `?2` wallets with an id after `?1`, by id.
        diesel::sql_query(
            r#"
            SELECT
                w.id,
                w.chain,
                w.network,
                w.address,
                w.secret IS NOT NULL AS keyed,
                w.tags,
                w.external_id,
                COALESCE((
                    SELECT json_group_object(
                        wi.asset,
                        json_object(
                            'id', wi.id,
                            'wallet_id', wi.wallet_id,
                            'asset', wi.asset,
                            'requested_from', wi.requested_from,
                            'birthday', wi.birthday,
                            'indexed_at', wi.indexed_at,
                            'state', wi.state,
                            'reason', wi.reason,
                            'indexed_through', wi.indexed_through,
                            'behind_tip', wi.behind_tip,
                            'finality_at', wi.finality_at,
                            'balance_confirmed', wi.balance_confirmed,
                            'balance_confirmed_received', wi.balance_confirmed_received,
                            'balance_confirmed_sent', wi.balance_confirmed_sent,
                            'balance_pending', wi.balance_pending,
                            'keyed', json(iif(wi.keyed, 'true', 'false')),
                            'balance_confirmed_spendable', wi.balance_confirmed_spendable
                        )
                    )
                    FROM wallet_index_with_balance wi
                    WHERE wi.wallet_id = w.id
                ), '{}') AS indexes
            FROM wallet w
            WHERE w.id > ?1
            ORDER BY w.id
            LIMIT ?2"#,
        )
        .bind::<Text, _>(page.after.unwrap_or_default())
        .bind::<BigInt, _>(page.limit)
        .load::<AtTip>(connection)?
        .into_iter()
        .map(Wallet::try_from)
        .collect()
    }

    /// Keeps `wallet` without its tags: a new row, or, for a kept row, its
    /// key if it was watch-only and its `external_id` if it had none (its
    /// tags and indexes stay); a kept key or `external_id` stays.
    /// Whether the row is new.
    pub(crate) fn insert(connection: &mut SqliteConnection, wallet: &Wallet) -> QueryResult<bool> {
        let secret = wallet.secret.0.as_ref().map(|key| key.as_bytes());
        // A new row; nothing when its id is kept.
        let created = diesel::sql_query(
            "
            INSERT INTO wallet (
                id,
                chain,
                network,
                address,
                curve,
                secret,
                external_id
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT (id) DO NOTHING",
        )
        .bind::<Text, _>(&wallet.id)
        .bind::<Text, _>(&wallet.chain)
        .bind::<Text, _>(&wallet.network)
        .bind::<Text, _>(&wallet.address)
        .bind::<Nullable<Text>, _>(wallet.curve.as_deref())
        .bind::<Nullable<Binary>, _>(secret)
        .bind::<Nullable<Text>, _>(wallet.external_id.as_deref())
        .execute(connection)?
            > 0;
        if !created {
            // The key `?2`, `?3` (if any) of kept watch-only row `?1`; a kept key
            // stays.
            diesel::sql_query(
                "
                UPDATE wallet
                SET curve = ?2,
                    secret = ?3
                WHERE id = ?1
                  AND secret IS NULL
                  AND ?3 IS NOT NULL",
            )
            .bind::<Text, _>(&wallet.id)
            .bind::<Nullable<Text>, _>(wallet.curve.as_deref())
            .bind::<Nullable<Binary>, _>(secret)
            .execute(connection)?;
            // The external id `?2` (if any) of kept row `?1` that has none; a
            // kept one stays.
            diesel::sql_query(
                "
                UPDATE wallet
                SET external_id = ?2
                WHERE id = ?1
                  AND external_id IS NULL
                  AND ?2 IS NOT NULL",
            )
            .bind::<Text, _>(&wallet.id)
            .bind::<Nullable<Text>, _>(wallet.external_id.as_deref())
            .execute(connection)?;
        }
        Ok(created)
    }

    /// Every wallet whose key is kept, indexes not read.
    pub(crate) fn find_keyed(connection: &mut SqliteConnection) -> QueryResult<Wallets> {
        let rows = wallet::table
            .filter(wallet::secret.is_not_null())
            .order(wallet::id)
            .select(Row::as_select())
            .load(connection)?;
        Ok(rows.into_iter().map(Wallet::from).collect())
    }

    /// Whether wallet `id` is kept with its key.
    pub(crate) fn is_keyed(
        connection: &mut SqliteConnection,
        id: impl AsRef<str>,
    ) -> QueryResult<bool> {
        diesel::select(diesel::dsl::exists(
            wallet::table
                .find(id.as_ref())
                .filter(wallet::secret.is_not_null()),
        ))
        .get_result(connection)
    }

    /// How many wallets of every network keep their key.
    pub(crate) fn count_keyed(connection: &mut SqliteConnection) -> QueryResult<i64> {
        wallet::table
            .filter(wallet::secret.is_not_null())
            .count()
            .get_result(connection)
    }

    /// A tag in use.
    #[derive(QueryableByName)]
    struct InUse {
        #[diesel(sql_type = Text)]
        tag: String,
    }

    /// At most `page.limit` tags in use on any wallet after `page.after`
    /// when given, sorted, each once. Reads only the tags (their index).
    pub(crate) fn find_tags_page(
        connection: &mut SqliteConnection,
        page: Page<String>,
    ) -> QueryResult<Vec<String>> {
        let rows: Vec<InUse> = diesel::sql_query(
            "
            SELECT DISTINCT
                j.value AS tag
            FROM wallet w, json_each(w.tags) j
            WHERE j.value > ?1
            ORDER BY tag
            LIMIT ?2",
        )
        .bind::<Text, _>(page.after.unwrap_or_default())
        .bind::<BigInt, _>(page.limit)
        .load(connection)?;
        Ok(rows.into_iter().map(|row| row.tag).collect())
    }

    /// How many tags every wallet carries, summed.
    pub(crate) fn count_tags(connection: &mut SqliteConnection) -> QueryResult<i64> {
        wallet::table
            .select(sql::<BigInt>("COALESCE(SUM(json_array_length(tags)), 0)"))
            .get_result(connection)
    }

    /// Adds (`add`) or removes `tag` on wallet `id`; `None` when no such
    /// wallet, else whether its tags changed.
    ///
    /// # Errors
    ///
    /// When `tag` is not [`is_tag`], or the read or write fails.
    pub(crate) fn set_tag(
        connection: &mut SqliteConnection,
        id: impl AsRef<str>,
        tag: &str,
        add: bool,
    ) -> anyhow::Result<Option<bool>> {
        let id = id.as_ref();
        ensure!(is_tag(tag), "a tag is 1 to 64 of [a-z0-9-]: {tag:?}");
        let Some(Json(mut tags)) = wallet::table
            .find(id)
            .select(wallet::tags)
            .first::<Json<std::collections::BTreeSet<String>>>(connection)
            .optional()?
        else {
            return Ok(None);
        };
        let changed = if add {
            tags.insert(tag.to_owned())
        } else {
            tags.remove(tag)
        };
        if changed {
            diesel::update(wallet::table.find(id))
                .set(wallet::tags.eq(Json(tags)))
                .execute(connection)?;
        }
        Ok(Some(changed))
    }
}
