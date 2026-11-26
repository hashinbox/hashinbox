//! Temp databases: a migrated file alone ([`fresh`]) or with the scripted
//! network's rows ([`seeded`]).

use diesel::connection::SimpleConnection as _;

use crate::{db::Db, usecase::sdk::Stores};

/// The file's name in its directory.
pub(crate) const FILE: &str = "test.db";

/// A new file with every schema applied, and the directory that holds it.
pub(crate) async fn fresh() -> (Db, tempfile::TempDir) {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let db = Db::open_or_create(&directory.path().join(FILE))
        .await
        .expect("the file is created");
    (db, directory)
}

/// [`fresh`], with the scripted network: its `network` and native
/// `network_asset` rows (no `network_index` row: no tip known yet); and
/// the stores on it.
pub(crate) async fn seeded() -> (Stores, Db, tempfile::TempDir) {
    let (db, directory) = fresh().await;
    db.write(|connection| {
        Ok(connection.batch_execute(
            "
            INSERT INTO network (chain, network, testnet, enabled, block_time_ms, confirmations)
            VALUES ('bitcoin', 'regtest', 1, 1, 600000, 0);
            INSERT INTO network_asset (chain, network, asset, kind, ticker, decimals)
            VALUES ('bitcoin', 'regtest', 'native', 'native', 'BTC', 8);",
        )?)
    })
    .await
    .expect("the scripted network is seeded");
    (Stores::new(db.clone(), Default::default()), db, directory)
}
