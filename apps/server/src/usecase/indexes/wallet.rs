//! Wallet indexes over the `leg` model: what an index's legs become when it
//! changes; each model writes its own table, the order lives here.

use base::Decimal;
use diesel::SqliteConnection;

use crate::models::{
    Leg, int,
    leg::{self, Legs},
    wallet_index::WalletIndexId,
};

/// Archives at `birthday`, the jump line, the coins index `id` still shows
/// unspent below it, and writes them off at `at`, the position it is
/// indexed through, or at its last leg below the line when that is above (a
/// send kept while it was paused): one adjustment (leg 1) of −Σ them, which
/// lowers its total only. None: nothing.
///
/// # Errors
///
/// When a value does not fit or decode, or a write fails.
pub(crate) fn write_off(
    connection: &mut SqliteConnection,
    id: &WalletIndexId,
    at: u64,
    birthday: u64,
) -> anyhow::Result<()> {
    let line = int(birthday)?;
    let Some(archived) = leg::db::archive(connection, id, line)? else {
        return Ok(());
    };
    let last = leg::db::find_last_position(connection, id, line)?;
    let position = int(at)?.max(last.unwrap_or(i64::MIN));
    let adjustment = Leg::adjustment(position, id, 1, &(Decimal::zero() - archived))?;
    leg::db::append(connection, &mut Legs::from(vec![adjustment]), position)
}
