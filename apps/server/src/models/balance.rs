//! `balance`: a wallet index's balance, folded from its legs; no table.

use std::collections::BTreeMap;

use base::Decimal;

/// The atomic balance of one wallet index at a confirmed tip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Balance {
    /// The sum of its legs in live blocks at or below the tip.
    pub(crate) confirmed: Decimal,
    /// What it can spend: `confirmed` less what its pending transactions
    /// take (spent, burned, fee). What they bring counts once mined.
    pub(crate) spendable: Decimal,
}

impl Default for Balance {
    fn default() -> Self {
        Self {
            confirmed: Decimal::zero(),
            spendable: Decimal::zero(),
        }
    }
}

/// Balances by wallet index id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Balances(BTreeMap<String, Balance>);

impl std::ops::Deref for Balances {
    type Target = BTreeMap<String, Balance>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl IntoIterator for Balances {
    type Item = (String, Balance);
    type IntoIter = std::collections::btree_map::IntoIter<String, Balance>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

/// The balance statements (over `wallet_index`, `leg`, `block` and `transaction`).
pub(crate) mod db {
    use anyhow::{Context as _, ensure};
    use diesel::{
        prelude::*,
        sql_types::{BigInt, Bool, Text},
    };

    use super::{Balance, Balances};
    use crate::models::Leg;

    /// The legs of wallet indexes `?1` (a JSON list of ids) of their network,
    /// in live blocks or pending, each marked confirmed when its block is at
    /// or below position `?2`.
    const LEGS: &str = r#"
    SELECT wi.id AS index_id, COALESCE(b.position <= ?2, 0) AS confirmed, l.*
    FROM wallet_index wi
    JOIN leg l ON l.address = wi.address AND l.asset = wi.asset
    LEFT JOIN block b ON b.id = l.block
    LEFT JOIN "transaction" t ON t.id = l."transaction"
    WHERE wi.id IN (SELECT value FROM json_each(?1))
      AND COALESCE(b.chain, t.chain) = wi.chain
      AND COALESCE(b.network, t.network) = wi.network
      AND (l.block IS NULL OR b.archived_at IS NULL)"#;

    /// A leg with the wallet index it counts for.
    #[derive(QueryableByName)]
    struct Counted {
        #[diesel(sql_type = Text)]
        index_id: String,
        #[diesel(sql_type = Bool)]
        confirmed: bool,
        #[diesel(embed)]
        leg: Leg,
    }

    /// The balances of wallet indexes `ids` at confirmed tip `confirmed`;
    /// every id is present, zero without legs.
    ///
    /// # Errors
    ///
    /// When the read fails, a stored value does not decode, or a confirmed
    /// balance is negative.
    pub(crate) fn find_by_ids(
        connection: &mut SqliteConnection,
        ids: &[&str],
        confirmed: i64,
    ) -> anyhow::Result<Balances> {
        let rows: Vec<Counted> = diesel::sql_query(LEGS)
            .bind::<Text, _>(serde_json::to_string(ids)?)
            .bind::<BigInt, _>(confirmed)
            .load(connection)?;
        let mut balances: std::collections::BTreeMap<String, Balance> = ids
            .iter()
            .map(|id| ((*id).to_owned(), Balance::default()))
            .collect();
        for row in rows {
            let signed = row.leg.signed()?;
            let balance = balances.entry(row.index_id).or_default();
            if row.confirmed {
                balance.confirmed += signed.clone();
                balance.spendable += signed;
            } else if row.leg.block.is_none() && signed < base::Decimal::zero() {
                balance.spendable += signed;
            }
        }
        ensure!(
            balances
                .values()
                .all(|balance| balance.confirmed >= base::Decimal::zero()),
            "a confirmed balance is negative"
        );
        Ok(Balances(balances))
    }

    /// The balance of wallet index `id` at confirmed tip `confirmed`.
    #[allow(dead_code, reason = "for the API; no caller yet")]
    ///
    /// # Errors
    ///
    /// As [`find_by_ids`].
    pub(crate) fn get_by_id(
        connection: &mut SqliteConnection,
        id: &str,
        confirmed: i64,
    ) -> anyhow::Result<Balance> {
        find_by_ids(connection, &[id], confirmed)?
            .0
            .remove(id)
            .context("a balance was not read")
    }
}
