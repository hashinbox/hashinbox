//! [`Targets`]: what targets name, each resolved once per evaluation.

use std::collections::BTreeMap;

use diesel::SqliteConnection;

use super::Target;
use crate::models::wallet_index_balance::{self, WalletIndexBalances};

/// Resolved targets by (`This` ids, target JSON); `None`: not fully indexed.
#[derive(Default)]
pub(crate) struct Targets(BTreeMap<(Vec<String>, String), Option<WalletIndexBalances>>);

impl Targets {
    /// What `target` names with `this` in context, with balances; `None`
    /// when one is not fully indexed.
    ///
    /// # Errors
    ///
    /// When a read fails or a stored value does not decode.
    pub(crate) fn find(
        &mut self,
        connection: &mut SqliteConnection,
        this: &WalletIndexBalances,
        target: &Target,
    ) -> anyhow::Result<Option<WalletIndexBalances>> {
        let key = (this.ids(), serde_json::to_string(target)?);
        if let Some(found) = self.0.get(&key) {
            return Ok(found.clone());
        }
        let found = wallet_index_balance::db::find_by_target(connection, this, target)?;
        self.0.insert(key, found.clone());
        Ok(found)
    }
}
