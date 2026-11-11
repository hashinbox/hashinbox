//! `base::Holdings` on the `wallet_index` model: what the index knows of
//! who holds what, for pricing fees by recipient.

use std::collections::BTreeSet;

use base::{Address, Asset, Future, Holdings, Spends};

use super::Stores;
use crate::models::wallet_index;

impl Holdings for Stores {
    fn holds<'a>(&'a self, spends: &'a Spends) -> Future<'a, anyhow::Result<Vec<bool>>> {
        Box::pin(async move {
            let wanted: Vec<(Address, Asset)> = spends
                .recipients()
                .map(|(to, asset)| (to.clone(), asset.clone()))
                .collect();
            let held: BTreeSet<(Address, Asset)> = self
                .db
                .read(move |connection| {
                    let wanted = wanted.iter().map(|(to, asset)| (to, asset));
                    Ok(wallet_index::db::find_held(connection, wanted)?
                        .into_iter()
                        .collect())
                })
                .await?;
            Ok(spends
                .iter()
                .map(|spend| {
                    spend
                        .to
                        .as_ref()
                        .is_some_and(|to| held.contains(&(to.clone(), spend.asset.clone())))
                })
                .collect())
        })
    }
}
