//! Which triggers kept blocks concern: those whose selector picks a wallet
//! index with a leg in one of them.

use std::collections::BTreeSet;

use base::{BlockHeader, Network};
use diesel::SqliteConnection;

use crate::models::{Block, trigger, wallet_index, wallet_index::Selector};

/// The ids of `network`'s enabled triggers that `headers` (kept blocks of
/// it) touched.
///
/// # Errors
///
/// When a read fails or a hash does not make an id.
pub(crate) fn touched(
    connection: &mut SqliteConnection,
    network: &Network,
    headers: &[BlockHeader],
) -> anyhow::Result<BTreeSet<String>> {
    let blocks = headers
        .iter()
        .map(|header| Block::id(network, &header.hash))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let indexes = wallet_index::db::find_touched(connection, &blocks)?;
    let mut triggers = BTreeSet::new();
    if indexes.is_empty() {
        return Ok(triggers);
    }
    let changed = Selector::Ids(indexes);
    for one in trigger::db::find_enabled(connection, network)?.iter() {
        let both = Selector::All(vec![one.selector.0.clone(), changed.clone()]);
        // Indexed through any block: a change counts before the index reaches
        // the tip; whether it takes part is the evaluation's.
        if !wallet_index::db::find_matching(connection, network, &both)?.is_empty() {
            triggers.insert(one.id.clone());
        }
    }
    Ok(triggers)
}
