//! The server's `models` and stores (`usecase`) on SQLite temp files: the `base`
//! store contracts, and the balance read. Offline and
//! deterministic.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

use base::{
    Address, Asset, Birthday, Block, BlockHash, BlockHeader, BlockNotify, BlockParent, BlockStore,
    Blocks, Chain, Coin, Decimal, Holding, IndexState, Indexer, InputStore, Leg, Line, Network,
    Next, Progress, Reward, Transaction, TransactionId, TransactionState, WalletFactories,
    WalletFactoryBroadcaster, WalletIndex, WalletIndexStore, WalletIndexTarget, WalletStore,
    testing::{
        self, Broadcasts, Confirms, Held, Keys, Served,
        fixtures::{block, coin, header, payment, quiet, run},
        idle,
    },
};

use super::support::{
    db::seeded,
    scripted::{Script, limited, network},
};
use crate::{
    db::{Db, Page},
    models::transaction_index::Why,
    usecase::sdk::Stores,
};

/// The signed balance of `address` in `asset` over every live block, as the
/// balance route reads it.
async fn balance(db: &Db, address: &Address, asset: &Asset) -> anyhow::Result<Decimal> {
    let id = String::from(crate::models::wallet_index::WalletIndexId::of(
        address, asset,
    )?);
    db.read(move |connection| {
        let last = crate::models::leg::db::find_page(
            connection,
            &id,
            Page {
                after: None,
                limit: 1,
            },
        )?;
        match last.first().and_then(|row| row.leg.total.as_deref()) {
            Some(total) => Ok(total.parse()?),
            None => Ok(Decimal::zero()),
        }
    })
    .await
}

fn other() -> Network {
    Network::new(Chain::new("bitcoin"), "testnet")
}

pub(super) fn address(network: &Network, name: &str) -> Address {
    Address::new(network.clone(), name)
}

fn native(network: &Network) -> Asset {
    Asset::native(network.clone())
}

/// Empty blocks `from..=to` of branch `fork`.
fn empty(network: &Network, from: u64, to: u64, fork: u8) -> Blocks {
    run(
        network,
        (from..=to)
            .map(|p| block(network, p, fork, vec![]))
            .collect(),
    )
}

pub(super) async fn watch(storage: &Stores, address: &Address, birthday: u64) {
    let index = WalletIndex {
        address: address.clone(),
        asset: native(&address.network),
        requested_from: birthday,
        birthday,
        progress: Progress::default(),
    };
    WalletIndexStore::insert(storage, &index).await.unwrap();
}

async fn indexed_at(storage: &Stores, address: &Address) -> Option<u64> {
    let all = WalletIndexStore::find_all(storage, &address.network)
        .await
        .unwrap();
    let index = all
        .into_iter()
        .find(|index| index.address == *address)
        .unwrap();
    index.progress.indexed_at
}

/// The txids with a leg of `address`, pending or mined: mined ones
/// in chain order, then pending ones.
fn history(directory: &tempfile::TempDir, address: &Address) -> Vec<String> {
    use diesel::{Connection as _, RunQueryDsl as _, sql_types::Text};
    #[derive(diesel::QueryableByName)]
    struct Txid {
        #[diesel(sql_type = Text)]
        txid: String,
    }
    let path = directory.path().join(super::support::db::FILE);
    let mut connection = diesel::SqliteConnection::establish(path.to_str().unwrap()).unwrap();
    diesel::sql_query(
        r#"SELECT t.txid FROM "transaction" t LEFT JOIN block b ON b.id = t.block
        WHERE EXISTS (SELECT 1 FROM leg l WHERE l."transaction" = t.id AND l.address = ?)
        ORDER BY t.block IS NULL, b.position, t."order", t.id"#,
    )
    .bind::<Text, _>(&address.value)
    .load::<Txid>(&mut connection)
    .unwrap()
    .into_iter()
    .map(|row| row.txid)
    .collect()
}

#[tokio::test]
async fn insert_refuses_a_block_at_a_kept_position_and_accepts_gaps() {
    let (storage, _db, directory) = seeded().await;
    let network = network();
    BlockStore::insert(&storage, &empty(&network, 0, 3, 0))
        .await
        .unwrap();
    assert_eq!(
        storage.find_last(&network).await.unwrap(),
        Some(header(3, 0))
    );

    // Another block at a kept position: the unique index refuses it, and
    // nothing of the run is written.
    let error = BlockStore::insert(&storage, &empty(&network, 3, 4, 1))
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("block 3 conflicts with the kept one"),
        "{error:#}"
    );
    assert_eq!(
        storage.find_last(&network).await.unwrap(),
        Some(header(3, 0))
    );

    // The kept ones again are a no-op.
    BlockStore::insert(&storage, &empty(&network, 2, 5, 0))
        .await
        .unwrap();
    assert_eq!(
        storage.find_last(&network).await.unwrap(),
        Some(header(5, 0))
    );

    // A gap (pruned blocks) below a run is no conflict: 8 is kept unlinked.
    BlockStore::insert(&storage, &empty(&network, 8, 9, 0))
        .await
        .unwrap();
    assert!(blocks(&directory).contains(&(8, None)));
    // The gap's blocks link both sides.
    BlockStore::insert(&storage, &empty(&network, 6, 7, 0))
        .await
        .unwrap();
    let kept = blocks(&directory);
    assert!(kept.contains(&(6, Some(5))) && kept.contains(&(8, Some(7))));
}

/// Every block row of the test network by position: its position and its
/// stored parent's position.
fn blocks(directory: &tempfile::TempDir) -> Vec<(i64, Option<i64>)> {
    use diesel::{
        RunQueryDsl as _,
        sql_types::{BigInt, Nullable},
    };
    #[derive(diesel::QueryableByName)]
    struct Kept {
        #[diesel(sql_type = BigInt)]
        position: i64,
        #[diesel(sql_type = Nullable<BigInt>)]
        parent: Option<i64>,
    }
    diesel::sql_query(
        "SELECT b.position, p.position AS parent
        FROM block b LEFT JOIN block p ON p.id = b.parent
        WHERE b.chain = 'bitcoin' AND b.network = 'regtest'
        ORDER BY b.position",
    )
    .load::<Kept>(&mut raw(directory))
    .unwrap()
    .into_iter()
    .map(|kept| (kept.position, kept.parent))
    .collect()
}

#[tokio::test]
async fn insert_keeps_only_indexed_pairs_from_their_birthday() {
    let (storage, _db, directory) = seeded().await;
    let network = network();
    let alice = address(&network, "alice");
    let bob = address(&network, "bob");
    watch(&storage, &alice, 2).await;
    let blocks = run(
        &network,
        vec![
            block(&network, 0, 0, vec![]),
            block(
                &network,
                1,
                0,
                vec![payment(&network, "early", "alice", 5, None)],
            ),
            block(
                &network,
                2,
                0,
                vec![payment(&network, "a", "alice", 7, None)],
            ),
            block(&network, 3, 0, vec![payment(&network, "b", "bob", 9, None)]),
        ],
    );
    BlockStore::insert(&storage, &blocks).await.unwrap();
    assert_eq!(history(&directory, &alice), ["a"]);
    assert!(history(&directory, &bob).is_empty());
    assert_eq!(indexed_at(&storage, &alice).await, Some(3));
    // Another asset of a watched address is not kept.
    let token = Asset {
        network: network.clone(),
        id: "token".into(),
    };
    let mut tokens = payment(&network, "t", "alice", 1, None);
    tokens.legs = vec![Leg::Minted {
        address: alice.clone(),
        asset: token,
        amount: Decimal::from(1),
    }];
    BlockStore::insert(
        &storage,
        &run(&network, vec![block(&network, 4, 0, vec![tokens])]),
    )
    .await
    .unwrap();
    assert_eq!(history(&directory, &alice), ["a"]);
    assert_eq!(indexed_at(&storage, &alice).await, Some(4));
}

#[tokio::test]
async fn a_new_index_backfills_without_duplicating_other_facts() {
    let (storage, _db, directory) = seeded().await;
    let network = network();
    let alice = address(&network, "alice");
    let bob = address(&network, "bob");
    watch(&storage, &alice, 0).await;
    // One transaction pays both; only alice is indexed at first.
    let mut both = payment(&network, "x", "alice", 1, None);
    both.legs.push(Leg::Received {
        address: bob.clone(),
        asset: native(&network),
        amount: Decimal::from(2),
        coin: Some(coin(&network, "x", 1)),
    });
    let blocks = run(
        &network,
        vec![
            block(&network, 0, 0, vec![]),
            block(&network, 1, 0, vec![both]),
            block(&network, 2, 0, vec![]),
        ],
    );
    BlockStore::insert(&storage, &blocks).await.unwrap();
    assert_eq!(rows(&directory, "x").1.len(), 1);

    watch(&storage, &bob, 1).await;
    assert_eq!(
        storage.find_next(&network, 0, Some(2)).await.unwrap(),
        Some(1)
    );
    // The indexer refetches from bob's birthday.
    let rerun = run(&network, blocks.iter().skip(1).cloned().collect());
    BlockStore::insert(&storage, &rerun).await.unwrap();
    let found = rows(&directory, "x");
    assert_eq!(found.0.len(), 1, "one transaction row");
    assert_eq!(found.1.len(), 2, "bob's leg added once");
    assert_eq!(history(&directory, &bob), ["x"]);
    assert_eq!(indexed_at(&storage, &bob).await, Some(2));
    assert_eq!(storage.find_next(&network, 0, Some(2)).await.unwrap(), None);

    // A repeat is a no-op.
    BlockStore::insert(&storage, &rerun).await.unwrap();
    assert_eq!(rows(&directory, "x"), found);
}

#[tokio::test]
async fn a_new_index_below_the_kept_run_backfills_beneath_it() {
    let (storage, _db, directory) = seeded().await;
    let network = network();
    let alice = address(&network, "alice");
    watch(&storage, &address(&network, "bob"), 3).await;
    BlockStore::insert(&storage, &empty(&network, 3, 4, 0))
        .await
        .unwrap();
    watch(&storage, &alice, 1).await;
    let blocks = run(
        &network,
        vec![
            block(
                &network,
                1,
                0,
                vec![payment(&network, "a", "alice", 1, None)],
            ),
            block(&network, 2, 0, vec![]),
            block(&network, 3, 0, vec![]),
            block(&network, 4, 0, vec![]),
        ],
    );
    BlockStore::insert(&storage, &blocks).await.unwrap();
    assert_eq!(history(&directory, &alice), ["a"]);
    assert_eq!(indexed_at(&storage, &alice).await, Some(4));
}

#[tokio::test]
async fn delete_archives_unspends_and_rewinds() {
    let (storage, _db, directory) = seeded().await;
    let network = network();
    let alice = address(&network, "alice");
    watch(&storage, &alice, 0).await;
    let funded = run(
        &network,
        vec![
            block(&network, 0, 0, vec![]),
            block(
                &network,
                1,
                0,
                vec![payment(&network, "fund", "alice", 10, None)],
            ),
        ],
    );
    BlockStore::insert(&storage, &funded).await.unwrap();
    let unspent = storage
        .find_unspent(std::slice::from_ref(&alice))
        .await
        .unwrap();
    assert_eq!(unspent.len(), 1);
    assert_eq!(unspent[0].height, 1);

    let spend = payment(
        &network,
        "spend",
        "carol",
        10,
        Some(("alice", coin(&network, "fund", 0))),
    );
    BlockStore::insert(
        &storage,
        &run(&network, vec![block(&network, 2, 0, vec![spend])]),
    )
    .await
    .unwrap();
    assert!(
        storage
            .find_unspent(std::slice::from_ref(&alice))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(history(&directory, &alice), ["fund", "spend"]);

    BlockStore::delete(&storage, &network, 2).await.unwrap();
    assert_eq!(
        storage.find_last(&network).await.unwrap(),
        Some(header(1, 0))
    );
    assert_eq!(
        storage
            .find_unspent(std::slice::from_ref(&alice))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(history(&directory, &alice), ["fund"]);
    assert_eq!(indexed_at(&storage, &alice).await, Some(1));

    // The new branch is kept at the archived position.
    let branch = Block {
        header: BlockHeader {
            hash: BlockHash(vec![2, 7]),
            ..header(2, 0)
        },
        ..block(&network, 2, 0, vec![])
    };
    BlockStore::insert(&storage, &run(&network, vec![branch]))
        .await
        .unwrap();
    assert_eq!(indexed_at(&storage, &alice).await, Some(2));

    // Archiving everything rewinds below the birthday to unset.
    BlockStore::delete(&storage, &network, 0).await.unwrap();
    assert_eq!(storage.find_last(&network).await.unwrap(), None);
    assert_eq!(indexed_at(&storage, &alice).await, None);
    assert!(
        storage
            .find_unspent(std::slice::from_ref(&alice))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn networks_are_isolated() {
    let (storage, _db, _directory) = seeded().await;
    let (network, other) = (network(), other());
    watch(&storage, &address(&network, "alice"), 0).await;
    watch(&storage, &address(&other, "alice"), 0).await;
    BlockStore::insert(&storage, &empty(&network, 0, 3, 0))
        .await
        .unwrap();
    BlockStore::insert(&storage, &empty(&other, 0, 1, 0))
        .await
        .unwrap();
    BlockStore::delete(&storage, &network, 0).await.unwrap();
    assert_eq!(storage.find_last(&network).await.unwrap(), None);
    assert_eq!(storage.find_last(&other).await.unwrap(), Some(header(1, 0)));
    assert_eq!(
        indexed_at(&storage, &address(&other, "alice")).await,
        Some(1)
    );
}

#[tokio::test]
async fn find_next_is_the_lowest_lagging_index() {
    let (storage, _db, _directory) = seeded().await;
    let network = network();
    assert_eq!(storage.find_next(&network, 0, None).await.unwrap(), None);
    watch(&storage, &address(&network, "a"), 5).await;
    watch(&storage, &address(&network, "b"), 2).await;
    assert_eq!(storage.find_next(&network, 0, None).await.unwrap(), Some(2));
    assert_eq!(storage.find_next(&network, 3, None).await.unwrap(), Some(5));
    assert_eq!(storage.find_next(&network, 0, Some(1)).await.unwrap(), None);
    BlockStore::insert(&storage, &empty(&network, 2, 6, 0))
        .await
        .unwrap();
    assert_eq!(storage.find_next(&network, 0, None).await.unwrap(), Some(7));
    assert_eq!(storage.find_next(&network, 0, Some(6)).await.unwrap(), None);
}

#[tokio::test]
async fn find_behind_is_the_indexing_ones_below_furthest_behind_first() {
    let (storage, _db, _directory) = seeded().await;
    let network = network();
    let behind = |below: u64, limit: usize| {
        let storage = &storage;
        let network = &network;
        async move {
            let found = storage.find_behind(network, below, limit).await.unwrap();
            found
                .into_iter()
                .map(|index| index.address.value)
                .collect::<Vec<_>>()
        }
    };
    watch(&storage, &address(&network, "a"), 5).await;
    watch(&storage, &address(&network, "b"), 2).await;
    watch(&storage, &address(&network, "c"), 3).await;
    set_state(&storage, "c", IndexState::Paused).await;
    assert_eq!(behind(6, usize::MAX).await, ["b", "a"]);
    assert_eq!(behind(6, 1).await, ["b"]);
    assert_eq!(behind(2, usize::MAX).await, Vec::<String>::new());
    BlockStore::insert(&storage, &empty(&network, 2, 6, 0))
        .await
        .unwrap();
    assert_eq!(behind(7, usize::MAX).await, Vec::<String>::new());
    assert_eq!(behind(8, usize::MAX).await, ["a", "b"]);
}

#[tokio::test]
async fn notifying_storage_tells_listeners_after_commit() {
    let (storage, _db, _directory) = seeded().await;
    let storage = base::Notifying::new(storage);
    let network = network();
    let heard = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&heard);
    storage.subscribe(Box::new(move |blocks| {
        let positions: Vec<u64> = blocks.iter().map(|block| block.header.position).collect();
        sink.lock().unwrap().push(positions);
    }));
    BlockStore::insert(&storage, &empty(&network, 0, 2, 0))
        .await
        .unwrap();
    BlockStore::insert(&storage, &empty(&network, 0, 2, 0))
        .await
        .unwrap();
    // Once per insert, with its run; a repeat is heard again: listeners are hints.
    assert_eq!(*heard.lock().unwrap(), [[0, 1, 2], [0, 1, 2]]);
}

#[tokio::test]
async fn pending_transactions_are_kept_until_mined() {
    let (storage, db, directory) = seeded().await;
    let network = network();
    let alice = address(&network, "alice");
    watch(&storage, &alice, 0).await;
    let mut pending = payment(&network, "p", "alice", 3, None);
    pending.state = TransactionState::Pending;
    pending.order = None;
    record(&db, &pending).await;
    record(&db, &pending).await;
    assert_eq!(history(&directory, &alice), ["p"]);
    assert_eq!(rows(&directory, "p").0, [None]);
    let mined = payment(&network, "p", "alice", 3, None);
    BlockStore::insert(
        &storage,
        &run(&network, vec![block(&network, 0, 0, vec![mined.clone()])]),
    )
    .await
    .unwrap();
    assert_eq!(history(&directory, &alice), ["p"]);
    assert_eq!(
        rows(&directory, "p").0,
        [Some("bitcoin.regtest.0000".to_owned())]
    );
}

/// Each view reads through its `table!`: the declarations match its columns.
#[tokio::test]
async fn the_views_read_through_their_tables() {
    use crate::models::{schema::transaction_with_legs, wallet_index::db::IndexRow};
    use diesel::RunQueryDsl as _;
    let (storage, db, directory) = seeded().await;
    let network = network();
    watch(&storage, &address(&network, "alice"), 0).await;
    let mut pending = payment(&network, "p", "alice", 3, None);
    pending.state = TransactionState::Pending;
    pending.order = None;
    record(&db, &pending).await;
    let mut connection = raw(&directory);
    let indexes: Vec<IndexRow> = diesel::sql_query("SELECT * FROM wallet_index_with_balance")
        .load(&mut connection)
        .unwrap();
    assert_eq!(indexes.len(), 1);
    let transactions: Vec<crate::models::transaction::Transaction> =
        transaction_with_legs::table.load(&mut connection).unwrap();
    assert_eq!(transactions[0].legs.len(), 1);
}

/// A network with no `network_index` row and no node: no index, `[]`.
#[tokio::test]
async fn network_state_of_a_network_with_no_index_and_no_node() {
    use diesel::RunQueryDsl as _;
    let (_, db, directory) = seeded().await;
    diesel::sql_query(
        "INSERT INTO network (chain, network, testnet, enabled, block_time_ms, confirmations)
         VALUES ('bare', 'net', 1, 1, 1000, 1)",
    )
    .execute(&mut raw(&directory))
    .unwrap();
    let bare = Network::new(Chain::new("bare"), "net");
    let found = db.read(move |connection| crate::models::network::db::find(connection, &bare));
    let network = found.await.unwrap().expect("its row");
    assert!(network.enabled);
    assert!(network.index.is_none());
    assert!(network.assets.is_empty() && network.routes.is_empty());
    assert!(network.endpoints.is_empty());
    assert_eq!(network.endpoints.enabled(), 0);
}

/// The `transaction` rows of `txid`: their block ids, and the block ids of
/// their legs.
pub(super) fn rows(
    directory: &tempfile::TempDir,
    txid: &str,
) -> (Vec<Option<String>>, Vec<Option<String>>) {
    use diesel::{Connection as _, RunQueryDsl as _, sql_types::Text};
    let path = directory.path().join(super::support::db::FILE);
    let mut connection = diesel::SqliteConnection::establish(path.to_str().unwrap()).unwrap();
    let blocks = diesel::sql_query(r#"SELECT block FROM "transaction" WHERE txid = ?"#)
        .bind::<Text, _>(txid)
        .load::<Row>(&mut connection)
        .unwrap();
    let legs =
        diesel::sql_query(r#"SELECT block FROM leg WHERE "transaction" = ? ORDER BY leg_index"#)
            .bind::<Text, _>(format!("bitcoin.regtest.{txid}"))
            .load::<Row>(&mut connection)
            .unwrap();
    (
        blocks.into_iter().map(|row| row.block).collect(),
        legs.into_iter().map(|row| row.block).collect(),
    )
}

#[derive(diesel::QueryableByName)]
struct Row {
    #[diesel(sql_type = diesel::sql_types::Nullable<diesel::sql_types::Text>)]
    block: Option<String>,
}

/// An indexer step at `tip`, confirmed through `confirmed`.
fn step(tip: u64, confirmed: u64) -> crate::models::NetworkIndex {
    crate::models::NetworkIndex {
        tip,
        confirmed,
        ..Default::default()
    }
}

/// The indexer row at `tip`, every block confirmed: what spend and expiry
/// checks read.
pub(super) async fn confirm(db: &Db, tip: u64) {
    db.write(move |connection| {
        crate::models::network_index::db::save(connection, &network(), &step(tip, tip))
    })
    .await
    .unwrap();
}

/// Records `transaction` as pending, as the queued broadcaster does.
pub(super) async fn record(db: &Db, transaction: &Transaction) {
    let transaction = transaction.clone();
    db.write(move |connection| {
        let mut pending = crate::models::transaction::Transaction::try_from(&transaction)?;
        crate::models::transaction::db::save_pending(connection, &mut pending, 0)
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_pending_spend_holds_its_coin_until_it_is_forgotten() {
    let (storage, db, directory) = seeded().await;
    let network = network();
    let alice = address(&network, "alice");
    watch(&storage, &alice, 0).await;
    let funding = payment(&network, "f", "alice", 5, None);
    BlockStore::insert(
        &storage,
        &run(&network, vec![block(&network, 0, 0, vec![funding])]),
    )
    .await
    .unwrap();
    confirm(&db, 0).await;
    let unspent = |storage: Stores| {
        let alice = alice.clone();
        async move {
            storage
                .find_unspent(std::slice::from_ref(&alice))
                .await
                .unwrap()
                .len()
        }
    };
    assert_eq!(unspent(storage.clone()).await, 1);
    let mut spend = pending(&network, "s", "bob", 5);
    spend.legs.push(Leg::Spent {
        address: alice.clone(),
        asset: native(&network),
        amount: Decimal::from(5_u64),
        coin: Some(coin(&network, "f", 0)),
    });
    record(&db, &spend).await;
    assert_eq!(unspent(storage.clone()).await, 0);
    assert_eq!(forget(&db, "s", Why::Refused).await, 1);
    assert_eq!(unspent(storage.clone()).await, 1);
    assert_eq!(rows(&directory, "s"), (vec![], vec![]), "row and legs gone");
    assert_eq!(forget(&db, "s", Why::Refused).await, 0, "forgotten once");
}

/// Forgets pending transaction `txid` for `why`, as the jobs do; how many.
pub(super) async fn forget(db: &Db, txid: &str, why: Why) -> usize {
    let id = format!("bitcoin.regtest.{txid}");
    db.write(move |connection| crate::usecase::indexes::transaction::forget(connection, &[id], why))
        .await
        .unwrap()
}

fn pending(network: &Network, txid: &str, to: &str, amount: u64) -> Transaction {
    let mut transaction = payment(network, txid, to, amount, None);
    transaction.state = TransactionState::Pending;
    transaction.order = None;
    transaction
}

#[tokio::test]
async fn mining_upserts_the_pending_row_and_its_legs() {
    let (storage, db, directory) = seeded().await;
    let network = network();
    let alice = address(&network, "alice");
    watch(&storage, &alice, 0).await;
    record(&db, &pending(&network, "p", "alice", 3)).await;
    assert_eq!(rows(&directory, "p"), (vec![None], vec![None]));
    let mined = payment(&network, "p", "alice", 3, None);
    BlockStore::insert(
        &storage,
        &run(&network, vec![block(&network, 0, 0, vec![mined.clone()])]),
    )
    .await
    .unwrap();
    let block0 = Some("bitcoin.regtest.0000".to_owned());
    assert_eq!(
        rows(&directory, "p"),
        (vec![block0.clone()], vec![block0.clone()])
    );
    // A pending insert after mining is a no-op.
    record(&db, &pending(&network, "p", "alice", 9)).await;
    assert_eq!(history(&directory, &alice), ["p"]);
    assert_eq!(rows(&directory, "p").0, [block0]);
}

#[tokio::test]
async fn a_reorged_transaction_moves_to_its_new_block() {
    let (storage, db, directory) = seeded().await;
    let network = network();
    let alice = address(&network, "alice");
    watch(&storage, &alice, 0).await;
    let mined = payment(&network, "m", "alice", 4, None);
    BlockStore::insert(&storage, &empty(&network, 0, 0, 0))
        .await
        .unwrap();
    BlockStore::insert(
        &storage,
        &run(&network, vec![block(&network, 1, 0, vec![mined.clone()])]),
    )
    .await
    .unwrap();
    BlockStore::delete(&storage, &network, 1).await.unwrap();
    // Archived and not re-mined: not shown, not counted.
    assert!(history(&directory, &alice).is_empty());
    assert_eq!(
        balance(&db, &alice, &native(&network)).await.unwrap(),
        Decimal::zero()
    );
    let mut branch = block(&network, 1, 1, vec![mined.clone()]);
    branch.header.parent = header(1, 0).parent;
    BlockStore::insert(&storage, &run(&network, vec![branch]))
        .await
        .unwrap();
    let moved = Some("bitcoin.regtest.0101".to_owned());
    assert_eq!(rows(&directory, "m"), (vec![moved.clone()], vec![moved]));
    assert_eq!(history(&directory, &alice), ["m"]);
    assert_eq!(
        balance(&db, &alice, &native(&network)).await.unwrap(),
        Decimal::from(4_u64)
    );
}

/// A reorg from 2 keeps this server's send s (`broadcasted_at`) pending
/// with its legs, and deletes what is not ours at or above 2: o with its
/// legs, the block's reward (with the block) and alice's adjustment at 3
/// (design 087). Alice, settled at 3 (her new birthday), is capped below
/// it: unset, she reads (settles) again.
#[tokio::test]
async fn a_reorg_keeps_ours_pending_and_deletes_the_rest() {
    use diesel::{RunQueryDsl as _, sql_types::BigInt};
    #[derive(diesel::QueryableByName)]
    struct Count {
        #[diesel(sql_type = BigInt)]
        count: i64,
    }
    let (storage, db, directory) = seeded().await;
    let count = |sql: &str| {
        diesel::sql_query(sql)
            .get_result::<Count>(&mut raw(&directory))
            .unwrap()
            .count
    };
    let network = network();
    let alice = address(&network, "alice");
    watch(&storage, &alice, 0).await;
    BlockStore::insert(&storage, &empty(&network, 0, 1, 0))
        .await
        .unwrap();
    record(&db, &pending(&network, "s", "alice", 3)).await;
    let mut two = block(
        &network,
        2,
        0,
        vec![
            payment(&network, "s", "alice", 3, None),
            payment(&network, "o", "alice", 4, None),
        ],
    );
    two.rewards = vec![Reward {
        index: 0,
        address: alice.clone(),
        asset: native(&network),
        amount: Decimal::from(50),
    }];
    BlockStore::insert(&storage, &run(&network, vec![two]))
        .await
        .unwrap();
    let index = WalletIndexStore::find_all(&storage, &network)
        .await
        .unwrap()
        .remove(0);
    let held = Holding {
        position: 3,
        amount: Decimal::from(58_u64),
    };
    assert!(
        WalletIndexStore::settle(&storage, &index, &held)
            .await
            .unwrap()
    );
    assert_eq!(adjustments(&directory), [(3, None)]);

    BlockStore::delete(&storage, &network, 2).await.unwrap();
    assert_eq!(rows(&directory, "s"), (vec![None], vec![None]), "pending");
    assert_eq!(
        count(
            r#"SELECT COUNT(*) AS count FROM "transaction" WHERE txid = 's' AND broadcasted_at = 0"#
        ),
        1,
        "still ours"
    );
    assert_eq!(rows(&directory, "o"), (vec![], vec![]), "deleted");
    assert_eq!(count("SELECT COUNT(*) AS count FROM leg WHERE kind = 5"), 0);
    assert_eq!(adjustments(&directory), []);
    assert_eq!(history(&directory, &alice), ["s"]);
    assert_eq!(indexed_at(&storage, &alice).await, None);
}

#[tokio::test]
async fn pending_legs_never_count() {
    let (storage, db, _directory) = seeded().await;
    let network = network();
    let alice = address(&network, "alice");
    watch(&storage, &alice, 0).await;
    record(&db, &pending(&network, "p", "alice", 3)).await;
    assert_eq!(
        balance(&db, &alice, &native(&network)).await.unwrap(),
        Decimal::zero()
    );
    assert!(
        storage
            .find_unspent(std::slice::from_ref(&alice))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn wallets_round_trip_through_their_factory() {
    let (storage, _db, _directory) = seeded().await;
    let network = network();
    let keys = Keys::new(network.clone());
    let mut factories = WalletFactories::new();
    factories.set(WalletFactoryBroadcaster {
        network: network.clone(),
        factory: Arc::new(keys.clone()),
        broadcaster: Arc::new(Broadcasts::default()),
    });
    WalletStore::insert(&storage, &keys.keyed("alice"))
        .await
        .unwrap();
    WalletStore::insert(&storage, &keys.keyed("bob"))
        .await
        .unwrap();
    let restored = WalletStore::find_all(&storage, &factories).await.unwrap();
    let mut addresses: Vec<_> = restored.iter().map(|w| w.address().value.clone()).collect();
    addresses.sort();
    assert_eq!(addresses, ["alice", "bob"]);
}

#[tokio::test]
async fn rewards_of_indexed_pairs_are_kept_once() {
    let (storage, _db, _directory) = seeded().await;
    let network = network();
    let alice = address(&network, "alice");
    watch(&storage, &alice, 0).await;
    let mut rewarded = block(&network, 0, 0, vec![]);
    rewarded.rewards = vec![Reward {
        index: 0,
        address: alice.clone(),
        asset: native(&network),
        amount: Decimal::from(50),
    }];
    let blocks = run(&network, vec![rewarded]);
    BlockStore::insert(&storage, &blocks).await.unwrap();
    BlockStore::insert(&storage, &blocks).await.unwrap();
    assert_eq!(indexed_at(&storage, &alice).await, Some(0));
}

#[tokio::test]
async fn balance_reads_live_blocks_only() {
    let (storage, db, _directory) = seeded().await;
    let network = network();
    let alice = address(&network, "alice");
    let asset = native(&network);
    watch(&storage, &alice, 0).await;
    let mut paid = block(
        &network,
        0,
        0,
        vec![payment(&network, "a", "alice", 5, None)],
    );
    paid.rewards = vec![Reward {
        index: 0,
        address: alice.clone(),
        asset: asset.clone(),
        amount: Decimal::from(1),
    }];
    let spent = payment(
        &network,
        "b",
        "bob",
        3,
        Some(("alice", coin(&network, "a", 0))),
    );
    let blocks = run(&network, vec![paid, block(&network, 1, 0, vec![spent])]);
    BlockStore::insert(&storage, &blocks).await.unwrap();
    assert_eq!(
        balance(&db, &alice, &asset).await.unwrap(),
        Decimal::from(3)
    );

    BlockStore::delete(&storage, &network, 1).await.unwrap();
    assert_eq!(
        balance(&db, &alice, &asset).await.unwrap(),
        Decimal::from(6)
    );
    let other = address(&other(), "alice");
    assert_eq!(
        balance(&db, &other, &native(&other.network)).await.unwrap(),
        Decimal::zero()
    );
}

/// A raw connection to the test file, foreign keys on.
pub(super) fn raw(directory: &tempfile::TempDir) -> diesel::SqliteConnection {
    use diesel::{Connection as _, connection::SimpleConnection as _};
    let path = directory.path().join(super::support::db::FILE);
    let mut connection = diesel::SqliteConnection::establish(path.to_str().unwrap()).unwrap();
    connection
        .batch_execute("PRAGMA foreign_keys = ON;")
        .unwrap();
    crate::db::register(&mut connection).unwrap();
    connection
}

#[tokio::test]
async fn the_database_refuses_a_parent_that_is_not_stored() {
    use diesel::connection::SimpleConnection as _;
    let (storage, _db, directory) = seeded().await;
    BlockStore::insert(&storage, &empty(&network(), 0, 0, 0))
        .await
        .unwrap();
    let mut connection = raw(&directory);
    // Block [1, 9] names a parent [0, 9] that is not stored.
    let orphan = "INSERT INTO block (id, chain, network, position, height, hash, parent, \
                  parent_position, parent_hash) VALUES ('bitcoin.regtest.0109', 'bitcoin', \
                  'regtest', 1, 1, x'0109', 'bitcoin.regtest.0009', 0, x'0009');";
    let error = connection.batch_execute(orphan).unwrap_err();
    assert!(error.to_string().contains("FOREIGN KEY"), "{error}");
    // The same block naming its stored parent is kept.
    let linked = "INSERT INTO block (id, chain, network, position, height, hash, parent, \
                  parent_position, parent_hash) VALUES ('bitcoin.regtest.0109', 'bitcoin', \
                  'regtest', 1, 1, x'0109', 'bitcoin.regtest.0000', 0, x'0000');";
    connection.batch_execute(linked).unwrap();
}

#[tokio::test]
async fn ids_are_dotted_natural_keys() {
    use diesel::{QueryableByName, RunQueryDsl as _, sql_types::Text};
    #[derive(QueryableByName)]
    struct Id {
        #[diesel(sql_type = Text)]
        id: String,
    }
    let (storage, _db, directory) = seeded().await;
    let network = network();
    let alice = address(&network, "alice");
    watch(&storage, &alice, 0).await;
    let mut paid = block(
        &network,
        0,
        0,
        vec![payment(&network, "aa", "alice", 5, None)],
    );
    paid.rewards = vec![Reward {
        index: 0,
        address: alice.clone(),
        asset: native(&network),
        amount: Decimal::from(1),
    }];
    BlockStore::insert(
        &storage,
        &run(&network, vec![paid, block(&network, 1, 0, vec![])]),
    )
    .await
    .unwrap();
    let mut connection = raw(&directory);
    let mut ids = |sql: &str| -> Vec<String> {
        diesel::sql_query(sql)
            .load::<Id>(&mut connection)
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect()
    };
    assert_eq!(
        ids("SELECT id FROM block ORDER BY position"),
        ["bitcoin.regtest.0000", "bitcoin.regtest.0100"]
    );
    assert_eq!(
        ids("SELECT parent AS id FROM block WHERE position = 1"),
        ["bitcoin.regtest.0000"]
    );
    assert_eq!(
        ids(r#"SELECT id FROM "transaction""#),
        ["bitcoin.regtest.aa"]
    );
    assert_eq!(
        ids("SELECT id FROM leg ORDER BY id"),
        ["bitcoin.regtest.0000.reward.0", "bitcoin.regtest.aa.0"]
    );
    assert_eq!(
        ids("SELECT id FROM wallet_index"),
        ["bitcoin.regtest.alice.native"]
    );
    // A part with a dot is refused before anything is written.
    let dotted = WalletIndex {
        address: address(&network, "a.b"),
        asset: native(&network),
        requested_from: 0,
        birthday: 0,
        progress: Progress::default(),
    };
    assert!(WalletIndexStore::insert(&storage, &dotted).await.is_err());
}

/// A transaction `txid` at `order` in its block with `legs`.
pub(super) fn mined(network: &Network, txid: &str, order: u64, legs: Vec<Leg>) -> Transaction {
    Transaction {
        id: TransactionId::new(network.clone(), txid),
        state: TransactionState::Succeeded,
        order: Some(order),
        legs,
        bytes: txid.as_bytes().to_vec(),
        expires_height: None,
    }
}

/// A send of `amount` from `from` to `to`, spending `coin`, paying `fee`.
pub(super) fn send(
    network: &Network,
    from: &str,
    to: &str,
    amount: u64,
    fee: u64,
    spent: Coin,
) -> Vec<Leg> {
    vec![
        Leg::Spent {
            address: address(network, from),
            asset: native(network),
            amount: Decimal::from(amount),
            coin: Some(spent),
        },
        Leg::Received {
            address: address(network, to),
            asset: native(network),
            amount: Decimal::from(amount),
            coin: None,
        },
        Leg::Fee {
            address: address(network, from),
            asset: native(network),
            amount: Decimal::from(fee),
        },
    ]
}

/// The guard of design 072: recomputes every live mined leg's running total
/// from the amounts, per index, in its order (position, transaction order,
/// leg index, rewards last; not the stored seq), checks each stored
/// total and index, and checks every balance read against the
/// recomputation: now (the balance route's read) and at each confirmed
/// position up to `tip` (the wallet read, the trigger read); spendable is
/// the confirmed balance less the outgoing legs not yet in it: pending, or
/// mined above the confirmed position.
async fn totals_hold(db: &Db, directory: &tempfile::TempDir, names: &[&str], tip: i64) {
    use diesel::{
        RunQueryDsl as _,
        sql_types::{BigInt, Integer, Text},
    };
    #[derive(diesel::QueryableByName)]
    struct Mined {
        #[diesel(sql_type = Text)]
        address: String,
        #[diesel(sql_type = Text)]
        wallet_index_id: String,
        #[diesel(sql_type = BigInt)]
        position: i64,
        #[diesel(sql_type = Integer)]
        kind: i32,
        #[diesel(sql_type = Text)]
        amount: String,
        #[diesel(sql_type = Text)]
        total: String,
        #[diesel(sql_type = Text)]
        total_received: String,
        #[diesel(sql_type = Text)]
        total_sent: String,
    }
    let rows: Vec<Mined> = diesel::sql_query(
        r#"SELECT l.address, l.wallet_index_id, l.position, l.kind, l.amount, l.total,
        l.total_received, l.total_sent FROM leg l
        JOIN block b ON b.id = l.block LEFT JOIN "transaction" t ON t.id = l."transaction"
        WHERE l.asset = 'native'
        ORDER BY l.wallet_index_id, b.position, l."transaction" IS NULL, t."order", l.leg_index"#,
    )
    .load(&mut raw(directory))
    .unwrap();
    assert!(!rows.is_empty());
    // Each address's [balance, received, sent] so far.
    let mut running = std::collections::BTreeMap::<String, [Decimal; 3]>::new();
    // (address, position, balance right after the leg), in order.
    let mut steps = Vec::new();
    // (address, position, amount) of every mined outgoing leg.
    let mut sends = Vec::new();
    for row in rows {
        assert_eq!(
            row.wallet_index_id,
            format!("bitcoin.regtest.{}.native", row.address)
        );
        let amount: Decimal = row.amount.parse().unwrap();
        let [total, received, sent] = running
            .entry(row.address.clone())
            .or_insert_with(|| [0; 3].map(Decimal::from));
        match row.kind {
            0 | 3 | 5 => *received += &amount,
            // An adjustment moves the balance only.
            6 => {}
            _ => *sent += &amount,
        }
        if matches!(row.kind, 1 | 2 | 4) {
            *total -= &amount;
            sends.push((row.address.clone(), row.position, amount));
        } else {
            *total += amount;
        }
        let stored = [row.total, row.total_received, row.total_sent]
            .map(|text| text.parse::<Decimal>().unwrap());
        let expected = [total.clone(), received.clone(), sent.clone()];
        assert_eq!(stored, expected, "{}", row.address);
        steps.push((row.address, row.position, expected));
    }
    // [balance, received, sent] of `name` through `through`.
    let totals = |name: &str, through: i64| {
        steps
            .iter()
            .rfind(|(address, position, _)| address == name && *position <= through)
            .map_or_else(
                || [0; 3].map(Decimal::from),
                |(_, _, totals)| totals.clone(),
            )
    };
    let at = |name: &str, through: i64| totals(name, through)[0].clone();
    // What each address's pending outgoing legs take.
    let pending: Vec<Mined> = diesel::sql_query(
        "SELECT l.address, l.wallet_index_id, 0 AS position, l.kind, l.amount, '0' AS total,
        '0' AS total_received, '0' AS total_sent FROM leg l
        WHERE l.position IS NULL AND l.asset = 'native'",
    )
    .load(&mut raw(directory))
    .unwrap();
    // What spendable takes off the balance through `through`.
    let out = |name: &str, through: i64| {
        let pending = pending
            .iter()
            .filter(|row| row.address == name && matches!(row.kind, 1 | 2 | 4))
            .fold(Decimal::zero(), |sum, row| {
                sum + row.amount.parse::<Decimal>().unwrap()
            });
        let mined = sends
            .iter()
            .filter(|(address, position, _)| address == name && *position > through)
            .fold(Decimal::zero(), |sum, (_, _, amount)| sum + amount.clone());
        pending + mined
    };
    // What each address's pending legs move, in and out.
    let in_flight = |name: &str| {
        pending
            .iter()
            .filter(|row| row.address == name)
            .fold(Decimal::zero(), |sum, row| {
                let amount = row.amount.parse::<Decimal>().unwrap();
                if matches!(row.kind, 1 | 2 | 4) {
                    sum - amount
                } else {
                    sum + amount
                }
            })
    };
    let network = network();
    for name in names {
        let now = balance(db, &address(&network, name), &native(&network))
            .await
            .unwrap();
        assert_eq!(now, at(name, i64::MAX), "{name} now");
    }
    // The wallet read at the tip, final block `through`: its balances; and
    // its mined legs' totals, paged latest first.
    for through in 0..=tip {
        let (tip, confirmed) = (u64::try_from(tip).unwrap(), u64::try_from(through).unwrap());
        let (found, legs) = db
            .write(move |connection| {
                let network = super::support::scripted::network();
                // Set as read at `through`, not stepped: a step never moves
                // the confirmed position back.
                diesel::sql_query(
                    "INSERT INTO network_index (chain, network, tip, confirmed) VALUES (?, ?, ?, ?) \
                     ON CONFLICT (chain, network) DO UPDATE \
                     SET tip = excluded.tip, confirmed = excluded.confirmed",
                )
                .bind::<diesel::sql_types::Text, _>(network.chain.as_str())
                .bind::<diesel::sql_types::Text, _>(&network.name)
                .bind::<diesel::sql_types::BigInt, _>(i64::try_from(tip)?)
                .bind::<diesel::sql_types::BigInt, _>(i64::try_from(confirmed)?)
                .execute(connection)?;
                let found = crate::models::wallet::db::find_page(
                    connection,
                    Page {
                        after: None,
                        limit: 100,
                    },
                )?;
                let legs = found
                    .iter()
                    .map(|one| {
                        let id = &one.indexes["native"].id;
                        crate::models::leg::db::find_page(
                            connection,
                            id,
                            Page {
                                after: None,
                                limit: 200,
                            },
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((found, legs))
            })
            .await
            .unwrap();
        assert_eq!(found.len(), names.len());
        for (one, legs) in found.iter().zip(legs) {
            let index = &one.indexes["native"];
            assert_eq!(index.finality_at, Some(through));
            let parse = |text: &str| text.parse::<Decimal>().unwrap();
            let read = [
                &index.balance_confirmed,
                &index.balance_confirmed_received,
                &index.balance_confirmed_sent,
            ]
            .map(|text| parse(text.as_deref().unwrap()));
            assert_eq!(
                read,
                totals(&one.address, through),
                "{} at {through}",
                one.address
            );
            let tip = parse(index.balance_pending.as_deref().unwrap());
            assert_eq!(
                tip,
                at(&one.address, i64::MAX) + in_flight(&one.address),
                "{} at the tip",
                one.address
            );
            let spendable = parse(index.balance_confirmed_spendable.as_deref().unwrap());
            let expected = at(&one.address, through) - out(&one.address, through);
            assert_eq!(
                spendable, expected,
                "{} spendable at {through}",
                one.address
            );
            let shown: Vec<Decimal> = legs
                .iter()
                .rev()
                .filter_map(|row| row.leg.total.as_deref().map(parse))
                .collect();
            let recomputed: Vec<Decimal> = steps
                .iter()
                .filter(|(address, _, _)| *address == one.address)
                .map(|(_, _, totals)| totals[0].clone())
                .collect();
            assert_eq!(shown, recomputed, "{} legs", one.address);
        }
    }
    let ids: Vec<String> = names
        .iter()
        .map(|name| format!("bitcoin.regtest.{name}.native"))
        .collect();
    // The round's read: balances at the last kept block, spendable less
    // the pending sends.
    let balances = db
        .read(move |connection| {
            crate::models::wallet_index::db::find_by_ids(
                connection,
                &super::support::scripted::network(),
                &ids,
                tip,
            )
        })
        .await
        .unwrap();
    for name in names {
        let id = format!("bitcoin.regtest.{name}.native");
        let last = balances[&id]
            .balance_confirmed
            .as_deref()
            .map(|total| total.parse::<Decimal>().unwrap());
        assert_eq!(last, Some(at(name, i64::MAX)), "{name} at the last block");
        let spendable = balances[&id].spendable().unwrap();
        assert_eq!(
            spendable,
            at(name, i64::MAX) - out(name, i64::MAX),
            "{name} spendable at the last block"
        );
    }
}

#[tokio::test]
async fn running_totals_match_the_amounts_through_reorgs_and_sends() {
    let (storage, db, directory) = seeded().await;
    let network = network();
    let names = ["alice", "bob", "carol"];
    for name in names {
        watch(&storage, &address(&network, name), 0).await;
    }
    let reward = |index, name, amount| Reward {
        index,
        address: address(&network, name),
        asset: native(&network),
        amount: Decimal::from(amount),
    };
    // Block 0: several indexes, two transactions, rewards (out of index order).
    let mut first = block(
        &network,
        0,
        0,
        vec![
            mined(
                &network,
                "f2",
                1,
                payment(&network, "f2", "bob", 7, None).legs,
            ),
            mined(
                &network,
                "f1",
                0,
                payment(&network, "f1", "alice", 10, None).legs,
            ),
        ],
    );
    first.rewards = vec![
        reward(1, "alice", 2),
        reward(0, "alice", 3),
        reward(2, "carol", 50),
    ];
    let second = block(
        &network,
        1,
        0,
        vec![mined(
            &network,
            "s",
            0,
            send(&network, "alice", "bob", 4, 1, coin(&network, "f1", 0)),
        )],
    );
    BlockStore::insert(&storage, &run(&network, vec![first, second]))
        .await
        .unwrap();
    totals_hold(&db, &directory, &names, 1).await;

    // A pending send, later mined with another fee; one forgotten.
    let mut pending_p = mined(
        &network,
        "p",
        0,
        send(&network, "alice", "carol", 2, 1, coin(&network, "f2", 0)),
    );
    pending_p.state = TransactionState::Pending;
    pending_p.order = None;
    record(&db, &pending_p).await;
    let mut pending_q = mined(
        &network,
        "q",
        0,
        send(&network, "alice", "bob", 1, 1, coin(&network, "s", 1)),
    );
    pending_q.state = TransactionState::Pending;
    pending_q.order = None;
    record(&db, &pending_q).await;
    // Pending legs: alice's sends lower her spendable only; bob's and
    // carol's incoming ones change nothing; no balance moves.
    totals_hold(&db, &directory, &names, 1).await;
    let spendable = |db: Db| async move {
        db.read(|connection| {
            crate::models::wallet_index::db::find_by_ids(
                connection,
                &super::support::scripted::network(),
                &["bitcoin.regtest.alice.native".to_owned()],
                1,
            )
        })
        .await
        .unwrap()["bitcoin.regtest.alice.native"]
            .spendable()
            .unwrap()
    };
    // 10 + 3 + 2 - 4 - 1, less 2 + 1 and 1 + 1 pending.
    assert_eq!(spendable(db.clone()).await, Decimal::from(5));
    assert_eq!(forget(&db, "q", Why::Refused).await, 1);
    assert_eq!(spendable(db.clone()).await, Decimal::from(7));
    let p_fee_2 = mined(
        &network,
        "p",
        0,
        send(&network, "alice", "carol", 2, 2, coin(&network, "f2", 0)),
    );
    let third = block(&network, 2, 0, vec![p_fee_2.clone()]);
    BlockStore::insert(&storage, &run(&network, vec![third.clone()]))
        .await
        .unwrap();
    // Mined above the confirmed block 1: not in its balance yet, still taken
    // off what is spendable (N2): 10 less 2 + 2.
    assert_eq!(spendable(db.clone()).await, Decimal::from(6));
    totals_hold(&db, &directory, &names, 2).await;

    // A reorg: block 2 archived; a branch mines p again (fee 3) one block later.
    BlockStore::delete(&storage, &network, 2).await.unwrap();
    totals_hold(&db, &directory, &names, 2).await;
    let mut branch = block(
        &network,
        2,
        1,
        vec![mined(
            &network,
            "b2",
            0,
            payment(&network, "b2", "carol", 9, None).legs,
        )],
    );
    branch.header.parent = header(2, 0).parent;
    let p_fee_3 = mined(
        &network,
        "p",
        1,
        send(&network, "alice", "carol", 2, 3, coin(&network, "f2", 0)),
    );
    let mut branch_next = block(&network, 3, 1, vec![p_fee_3]);
    branch_next.rewards = vec![reward(0, "bob", 1)];
    BlockStore::insert(&storage, &run(&network, vec![branch, branch_next]))
        .await
        .unwrap();
    totals_hold(&db, &directory, &names, 3).await;

    // The branch archived and block 2 revived (its hash again), then a new 3.
    BlockStore::delete(&storage, &network, 2).await.unwrap();
    let mut fourth = block(
        &network,
        3,
        0,
        vec![mined(
            &network,
            "g",
            0,
            payment(&network, "g", "bob", 6, None).legs,
        )],
    );
    fourth.rewards = vec![reward(0, "alice", 1)];
    BlockStore::insert(&storage, &run(&network, vec![third, fourth]))
        .await
        .unwrap();
    totals_hold(&db, &directory, &names, 3).await;
    // A second writer of an index's step is refused: its seq is taken.
    let taken = r#"INSERT INTO leg (id, block, "transaction", leg_index, kind, address, asset,
        amount, wallet_index_id, position, seq, total, total_received, total_sent)
        SELECT 'bitcoin.regtest.dup.0', block, "transaction", 9, kind, address, asset, amount,
          wallet_index_id, position, seq, total, total_received, total_sent
        FROM leg WHERE wallet_index_id = 'bitcoin.regtest.alice.native' LIMIT 1"#;
    assert!(diesel::RunQueryDsl::execute(diesel::sql_query(taken), &mut raw(&directory)).is_err());
    assert_eq!(
        balance(&db, &address(&network, "alice"), &native(&network))
            .await
            .unwrap(),
        // 10 + 3 + 2 - 4 - 1 - 2 - 2 + 1.
        Decimal::from(7)
    );
    let last = db
        .read(|connection| {
            let page = Page {
                after: None,
                limit: 1,
            };
            Ok(crate::models::leg::db::find_page(
                connection,
                "bitcoin.regtest.alice.native",
                page,
            )?)
        })
        .await
        .unwrap();
    let leg = &last[0].leg;
    // In: 10 + 3 + 2 + 1; out: sends 4 + 2, fees 1 + 2.
    let totals = (leg.total_received.as_deref(), leg.total_sent.as_deref());
    assert_eq!(totals, (Some("16"), Some("9")));
}

/// A pending send of `amount` from alice to bob paying `fee`.
fn pending_send(network: &Network, txid: &str, amount: u64, fee: u64) -> Transaction {
    let spent = coin(network, "f", u32::try_from(amount).unwrap());
    let mut transaction = mined(
        network,
        txid,
        0,
        send(network, "alice", "bob", amount, fee, spent),
    );
    transaction.state = TransactionState::Pending;
    transaction.order = None;
    transaction
}

/// Two pending sends of one index: (a) both mined in one block, out of
/// list order; (b) mined in two blocks; (c) one mined, one forgotten. The
/// totals hold and spendable (total less pending outgoing) follows.
#[tokio::test]
async fn two_pending_sends_of_one_index_land_in_the_totals() {
    let network = network();
    let names = ["alice", "bob"];
    let alice = "bitcoin.regtest.alice.native";
    for case in ['a', 'b', 'c'] {
        let (storage, db, directory) = seeded().await;
        for name in names {
            watch(&storage, &address(&network, name), 0).await;
        }
        let funding = payment(&network, "f", "alice", 20, None);
        BlockStore::insert(
            &storage,
            &run(&network, vec![block(&network, 0, 0, vec![funding])]),
        )
        .await
        .unwrap();
        confirm(&db, 0).await;
        record(&db, &pending_send(&network, "p1", 3, 1)).await;
        record(&db, &pending_send(&network, "p2", 5, 1)).await;
        let spendable = |db: Db| async move {
            db.read(|connection| {
                crate::models::wallet_index::db::find_by_ids(
                    connection,
                    &super::support::scripted::network(),
                    &[alice.to_owned()],
                    9,
                )
            })
            .await
            .unwrap()[alice]
                .spendable()
                .unwrap()
        };
        // 20 less 3 + 1 and 5 + 1 pending.
        assert_eq!(spendable(db.clone()).await, Decimal::from(10), "{case}");
        // Mined, each pays 2.
        let p1 = mined(&network, "p1", 1, pending_send(&network, "p1", 3, 2).legs);
        let p2 = mined(&network, "p2", 0, pending_send(&network, "p2", 5, 2).legs);
        match case {
            'a' => {
                // p2 is first in the block though listed second.
                let both = block(&network, 1, 0, vec![p1, p2]);
                BlockStore::insert(&storage, &run(&network, vec![both]))
                    .await
                    .unwrap();
                totals_hold(&db, &directory, &names, 1).await;
                assert_eq!(spendable(db.clone()).await, Decimal::from(8), "{case}");
            }
            'b' => {
                let first = block(&network, 1, 0, vec![p1]);
                BlockStore::insert(&storage, &run(&network, vec![first]))
                    .await
                    .unwrap();
                totals_hold(&db, &directory, &names, 1).await;
                // 20 - 5, less 5 + 1 pending.
                assert_eq!(spendable(db.clone()).await, Decimal::from(9), "{case}");
                let second = block(&network, 2, 0, vec![p2]);
                BlockStore::insert(&storage, &run(&network, vec![second]))
                    .await
                    .unwrap();
                totals_hold(&db, &directory, &names, 2).await;
                assert_eq!(spendable(db.clone()).await, Decimal::from(8), "{case}");
            }
            _ => {
                let first = block(&network, 1, 0, vec![p1]);
                BlockStore::insert(&storage, &run(&network, vec![first]))
                    .await
                    .unwrap();
                assert_eq!(forget(&db, "p2", Why::Refused).await, 1);
                totals_hold(&db, &directory, &names, 1).await;
                assert_eq!(spendable(db.clone()).await, Decimal::from(15), "{case}");
            }
        }
    }
}

/// A chain block as a model, no database: transactions by order, each leg
/// and reward of the block at its position, ids as the schema writes them.
#[test]
fn a_block_converts_with_its_transactions_legs_and_rewards() {
    let network = network();
    let mut chained = block(
        &network,
        2,
        0,
        vec![
            mined(
                &network,
                "b",
                1,
                send(&network, "alice", "bob", 3, 1, coin(&network, "a", 0)),
            ),
            payment(&network, "a", "alice", 5, None),
        ],
    );
    chained.rewards = vec![Reward {
        index: 0,
        address: address(&network, "carol"),
        asset: native(&network),
        amount: Decimal::from(50),
    }];
    let model = crate::models::Block::try_from(&chained).unwrap();
    assert_eq!(model.id.as_ref(), "bitcoin.regtest.0200");
    assert_eq!((model.position, model.parent.as_ref()), (2, None));
    let txids: Vec<_> = model.transactions.iter().map(|t| t.txid.as_str()).collect();
    assert_eq!(txids, ["a", "b"]);
    let b = &model.transactions[1];
    assert_eq!(
        (b.id.as_ref(), b.block.as_ref()),
        ("bitcoin.regtest.b", Some(&model.id))
    );
    let legs: Vec<_> = b
        .legs
        .iter()
        .map(|leg| (leg.id.as_str(), leg.kind, leg.wallet_index_id.as_ref()))
        .collect();
    assert_eq!(
        legs,
        [
            ("bitcoin.regtest.b.0", 1, "bitcoin.regtest.alice.native"),
            ("bitcoin.regtest.b.1", 0, "bitcoin.regtest.bob.native"),
            ("bitcoin.regtest.b.2", 4, "bitcoin.regtest.alice.native"),
        ]
    );
    let reward = &model.rewards[0];
    assert_eq!(
        (reward.id.as_str(), reward.kind, reward.amount.as_str()),
        ("bitcoin.regtest.0200.reward.0", 5, "50")
    );
    let every = b.legs.iter().chain(&model.rewards);
    for leg in every {
        assert_eq!(
            (leg.block.as_ref(), leg.position),
            (Some(&model.id), Some(2))
        );
        assert_eq!((leg.seq, leg.total.as_deref()), (None, None));
    }
}

/// A pending transaction as a model: no block, no position.
#[test]
fn a_pending_transaction_converts_without_a_block() {
    let network = network();
    let mut sent = payment(
        &network,
        "p",
        "bob",
        3,
        Some(("alice", coin(&network, "a", 0))),
    );
    sent.state = TransactionState::Pending;
    sent.order = None;
    let model = crate::models::transaction::Transaction::try_from(&sent).unwrap();
    assert_eq!(
        (model.block, model.order, model.state, model.broadcasted_at),
        (None, None, 0, None)
    );
    assert!(
        model
            .legs
            .iter()
            .all(|leg| leg.block.is_none() && leg.position.is_none())
    );
    assert_eq!(model.legs[1].coin_txid.as_deref(), Some("a"));
}

/// `keep` drops the legs of other indexes, then the transactions left
/// empty; rewards of other indexes too.
#[test]
fn keep_drops_legs_then_empty_transactions() {
    let network = network();
    let mut chained = block(
        &network,
        0,
        0,
        vec![
            payment(&network, "a", "alice", 5, None),
            mined(
                &network,
                "b",
                1,
                send(&network, "bob", "carol", 3, 1, coin(&network, "x", 0)),
            ),
        ],
    );
    chained.rewards = vec![Reward {
        index: 0,
        address: address(&network, "bob"),
        asset: native(&network),
        amount: Decimal::from(50),
    }];
    let mut model = crate::models::Block::try_from(&chained).unwrap();
    model.keep(
        &["bitcoin.regtest.alice.native".to_owned()].into(),
        &BTreeSet::new(),
        &BTreeSet::new(),
    );
    let kept: Vec<_> = model.transactions.iter().map(|t| t.txid.as_str()).collect();
    assert_eq!(kept, ["a"]);
    assert_eq!(model.transactions[0].legs.len(), 1);
    assert!(model.rewards.is_empty());
}

/// A block read plain, its transactions by order each with its legs, its
/// reward as a leg of its index; the mempool and one transaction likewise.
#[tokio::test]
async fn blocks_and_transactions_read_with_their_children() {
    let (storage, db, _directory) = seeded().await;
    let network = network();
    let alice = address(&network, "alice");
    watch(&storage, &alice, 0).await;
    let mut rewarded = block(
        &network,
        0,
        0,
        vec![
            mined(
                &network,
                "b",
                1,
                send(&network, "alice", "bob", 1, 1, coin(&network, "x", 0)),
            ),
            payment(&network, "a", "alice", 5, None),
        ],
    );
    rewarded.rewards = vec![Reward {
        index: 0,
        address: alice.clone(),
        asset: native(&network),
        amount: Decimal::from(50),
    }];
    BlockStore::insert(&storage, &run(&network, vec![rewarded]))
        .await
        .unwrap();
    record(&db, &pending(&network, "p", "alice", 3)).await;
    use crate::db::Paged as _;
    let (latest, at, mined, legs, mempool, one) = db
        .read(move |connection| {
            use crate::models::{block, leg, transaction};
            let network = super::support::scripted::network();
            fn first<K>(limit: i64) -> Page<K> {
                Page { after: None, limit }
            }
            let block = block::db::find_page(connection, &network, first(10))?;
            let id = block[0].id.clone();
            Ok((
                block.clone(),
                block::db::find_by_position(connection, &network, 0)?,
                transaction::db::find_by_block_page(connection, &id, first(100))?,
                leg::db::find_page(connection, "bitcoin.regtest.alice.native", first(1))?,
                transaction::db::find_pending_page(connection, &network, first(100))?,
                crate::models::transaction::db::get(connection, "bitcoin.regtest.b")?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(latest.len(), 1);
    let block = &latest[0];
    assert_eq!(
        at.into_iter().map(|at| at.id).collect::<Vec<_>>(),
        std::slice::from_ref(&block.id)
    );
    let txids: Vec<_> = mined.iter().map(|t| t.txid.as_str()).collect();
    assert_eq!(txids, ["a", "b"]);
    assert_eq!(mined[1].position, Some(0));
    // Only alice's legs are kept: b's spend and fee, in leg order.
    let kinds: Vec<_> = mined[1].legs.iter().map(|leg| leg.kind).collect();
    assert_eq!(kinds, [1, 4]);
    assert_eq!(
        (legs[0].leg.kind, legs[0].leg.total.as_deref()),
        (5, Some("53"))
    );
    let pending: Vec<_> = mempool.iter().map(|t| t.txid.as_str()).collect();
    assert_eq!(pending, ["p"]);
    assert_eq!(mempool[0].legs.len(), 1);
    assert_eq!(mempool.cursor(1), Some(format!("{}:0:0:p", i64::MAX)));
    let one = one.unwrap();
    assert_eq!((one.legs.len(), one.bytes.as_slice()), (2, b"b".as_slice()));
    record(&db, &self::pending(&network, "q", "alice", 1)).await;
    // Keyset pages of one, each after the last one's cursor.
    let (all, mempool, mined, below) = db
        .read(move |connection| {
            use crate::models::{block, transaction};
            let network = super::support::scripted::network();
            let id = block::db::find_by_position(connection, &network, 0)?
                .unwrap()
                .id;
            let below = block::db::find_page(
                connection,
                &network,
                Page {
                    after: Some(0),
                    limit: 10,
                },
            )?;
            let mut walk = |list: &str| -> anyhow::Result<Vec<String>> {
                let mut txids = Vec::new();
                let mut after = None;
                loop {
                    let page = Page { after, limit: 1 };
                    let page = match list {
                        "pending" => {
                            transaction::db::find_pending_page(connection, &network, page)?
                        }
                        "block" => transaction::db::find_by_block_page(connection, &id, page)?,
                        _ => transaction::db::find_page(connection, &network, page)?,
                    };
                    txids.extend(page.iter().map(|t| t.txid.clone()));
                    let Some(cursor) = page.cursor(1) else {
                        return Ok(txids);
                    };
                    after = Some(transaction::parse_key(&cursor).unwrap());
                }
            };
            Ok((walk("all")?, walk("pending")?, walk("block")?, below))
        })
        .await
        .unwrap();
    assert_eq!(all, ["q", "p", "b", "a"]);
    assert_eq!(mempool, ["p", "q"]);
    assert_eq!(mined, ["a", "b"]);
    assert!(below.is_empty());
}

#[tokio::test]
async fn the_network_index_is_fresh_only_when_its_tip_moves_and_counts_up() {
    use diesel::{QueryDsl as _, RunQueryDsl as _};

    use crate::models::{NetworkIndex, network_index::db::save, schema::network_index};
    let (_storage, db, _directory) = seeded().await;
    let rows = db
        .write(|connection| {
            let network = network();
            let mut rows = Vec::new();
            for (tip, indexed, now) in [(10, Some(4), 1), (10, None, 2), (12, None, 3)] {
                let step = NetworkIndex {
                    tip,
                    confirmed: tip.saturating_sub(6),
                    indexed,
                    checked_at: Some(now),
                    blocks: 1,
                    ..Default::default()
                };
                save(connection, &network, &step)?;
                rows.push(
                    network_index::table
                        .select((
                            network_index::confirmed,
                            network_index::indexed,
                            network_index::checked_at,
                            network_index::blocks,
                        ))
                        .first::<(i64, Option<i64>, Option<i64>, i64)>(connection)?,
                );
            }
            Ok(rows)
        })
        .await
        .unwrap();
    assert_eq!(
        rows,
        [
            (4, Some(4), Some(1), 1),
            (4, Some(4), Some(1), 2),
            (6, Some(4), Some(3), 3)
        ]
    );
}

#[tokio::test]
async fn the_network_index_moves_back_only_on_a_reorg() {
    use diesel::{QueryDsl as _, RunQueryDsl as _};

    use crate::models::{NetworkIndex, network_index::db::save, schema::network_index};
    let (_storage, db, _directory) = seeded().await;
    let rows = db
        .write(|connection| {
            let network = network();
            let mut rows = Vec::new();
            // A step at 12, one read from a node behind (no reorg), then a
            // reorg to a shorter chain.
            for (tip, reorgs, now) in [(12, 0, 1), (9, 0, 2), (9, 1, 3)] {
                let step = NetworkIndex {
                    tip,
                    confirmed: tip - 6,
                    checked_at: Some(now),
                    reorgs,
                    ..Default::default()
                };
                save(connection, &network, &step)?;
                rows.push(
                    network_index::table
                        .select((
                            network_index::tip,
                            network_index::confirmed,
                            network_index::checked_at,
                        ))
                        .first::<(i64, i64, Option<i64>)>(connection)?,
                );
            }
            Ok(rows)
        })
        .await
        .unwrap();
    assert_eq!(
        rows,
        [(12, 6, Some(1)), (12, 6, Some(1)), (9, 3, Some(3))],
        "a node behind keeps them, and does not look fresh; a reorg moves them"
    );
}

/// How far below its tip [`index`]'s chain is final: below every reorg the
/// tests make.
const FINALITY: u64 = 10;

/// An indexer over `chain` into `storage`, with no line.
pub(super) fn indexer(storage: &Stores, chain: Served) -> Indexer {
    testing::indexer(chain, Arc::new(storage.clone()), None, None)
}

/// An indexer over `chain` into `storage` whose line is the final block,
/// settling with `held`.
fn settling(storage: &Stores, chain: Served, held: Arc<Held>) -> Indexer {
    let chain = chain.with_line(Line::Final);
    testing::indexer(chain, Arc::new(storage.clone()), Some(held), None)
}

/// An indexer over `chain` into `storage` reading no more than 100
/// positions below the tip.
fn jumping(storage: &Stores, chain: Served) -> Indexer {
    let chain = chain.with_line(Line::Depth(100));
    testing::indexer(chain, Arc::new(storage.clone()), None, None)
}

/// Steps an indexer over `blocks` into `storage` until it is idle.
async fn index(storage: &Stores, blocks: &[Block]) {
    let chain = Served::new(blocks.to_vec(), FINALITY);
    idle(&indexer(storage, chain)).await;
}

/// The pruning tests' chain, 0..=16: alice is paid at 2 (coin a) and 9 and
/// pays bob coin a at 6; erin is paid at 4 and 7, carol at 5. From 7 on it
/// is branch 1; 7 links onto 6.
fn pruned_chain(network: &Network) -> Vec<Block> {
    let paid = |position: u64, txid: &str, to: &str, amount: u64| {
        block(
            network,
            position,
            u8::from(position >= 7),
            vec![payment(network, txid, to, amount, None)],
        )
    };
    let mut chain: Vec<Block> = (0..=16)
        .map(|position| block(network, position, u8::from(position >= 7), vec![]))
        .collect();
    chain[2] = paid(2, "a", "alice", 5);
    chain[4] = paid(4, "f", "erin", 3);
    chain[5] = paid(5, "e", "carol", 1);
    chain[6] = block(
        network,
        6,
        0,
        vec![payment(
            network,
            "c",
            "bob",
            5,
            Some(("alice", coin(network, "a", 0))),
        )],
    );
    chain[7] = paid(7, "g", "erin", 1);
    chain[7].header.parent = Some(BlockParent {
        position: 6,
        hash: header(6, 0).hash,
    });
    chain[9] = paid(9, "d", "alice", 2);
    chain
}

/// Indexes the pruning tests' history into `storage`: alice, bob, carol and
/// dave from 0; dave paused at 1; branch 0 through 8 (7 pays alice "r"),
/// carol's index forgotten (her transaction stays), then [`pruned_chain`],
/// reorging 7 and 8. The network prunes below 16 - 4 = 12.
async fn prunable(storage: &Stores, db: &Db) {
    let network = network();
    let chain = pruned_chain(&network);
    for name in ["alice", "bob", "carol", "dave"] {
        watch(storage, &address(&network, name), 0).await;
    }
    index(storage, &chain[..=1]).await;
    let dave = WalletIndexStore::find_all(storage, &network)
        .await
        .unwrap()
        .into_iter()
        .find(|index| index.address.value == "dave")
        .unwrap();
    WalletIndexStore::set_state(storage, &dave, base::IndexState::Paused, None)
        .await
        .unwrap();
    let mut forked = chain[..=6].to_vec();
    forked.push(block(
        &network,
        7,
        0,
        vec![payment(&network, "r", "alice", 4, None)],
    ));
    forked.push(block(&network, 8, 0, vec![]));
    index(storage, &forked).await;
    db.write(|connection| {
        use diesel::connection::SimpleConnection as _;
        crate::models::wallet_index::db::delete(connection, "bitcoin.regtest.carol.native")?;
        Ok(connection.batch_execute("UPDATE network SET max_depth = 4")?)
    })
    .await
    .unwrap();
    index(storage, &chain).await;
    confirm(db, 16).await;
}

/// What reads show of `names`: each index (balances included) with its
/// movements, and their unspent coins.
async fn reads(
    db: &Db,
    names: &'static [&str],
) -> (
    Vec<(
        Option<crate::models::wallet_index::WalletIndex>,
        serde_json::Value,
    )>,
    Vec<base::Input>,
) {
    use crate::models::{leg, wallet_index};
    db.read(move |connection| {
        let mut indexes = Vec::new();
        for name in names {
            let id = format!("bitcoin.regtest.{name}.native");
            let page = Page {
                after: None,
                limit: 100,
            };
            indexes.push((
                wallet_index::db::find_at_tip(connection, &id)?,
                serde_json::to_value(leg::db::find_page(connection, &id, page)?)?,
            ));
        }
        let unspent = leg::db::find_unspent(connection, &network(), names)?;
        Ok((indexes, unspent))
    })
    .await
    .unwrap()
}

/// Prunes the test network as the prune job does.
async fn prune(db: &Db) {
    let container = Arc::new(base::Container::new(Stores::storage(db.clone())));
    crate::usecase::workers::prune::Prune::new(db.clone(), container)
        .blocks()
        .await
        .unwrap();
}

#[tokio::test]
async fn pruning_deletes_empty_blocks_and_reads_the_same() {
    let (storage, db, directory) = seeded().await;
    prunable(&storage, &db).await;
    let names = &["alice", "bob", "carol", "dave"];
    let before = reads(&db, names).await;
    assert_eq!(before.1.len(), 2, "bob's coin c and alice's coin d");
    assert_eq!(rows(&directory, "r"), (vec![], vec![]), "the reorg took r");

    prune(&db).await;
    let after = blocks(&directory);
    // Below 12: 2, 6 and 9 (legs), 5 (carol's transaction); 0, 1 (dave's
    // indexed_at, a position), 3, 4, 7, 8, 10, 11 are gone. A child of a
    // deleted block has no parent.
    let mut kept = vec![(2, None), (5, None), (6, Some(5)), (9, None), (12, None)];
    kept.extend((13..=16).map(|position| (position, Some(position - 1))));
    assert_eq!(after, kept);
    assert_eq!(rows(&directory, "e").0.len(), 1, "a transaction stays");
    assert_eq!(reads(&db, names).await, before);

    prune(&db).await;
    assert_eq!(blocks(&directory), after, "pruning twice changes nothing");
    assert_eq!(reads(&db, names).await, before);

    // The last block stays, named by nothing.
    BlockStore::insert(&storage, &empty(&other(), 0, 3, 0))
        .await
        .unwrap();
    let deleted = db
        .write(|connection| Ok(crate::models::block::db::prune(connection, &other(), 10)?))
        .await
        .unwrap();
    assert_eq!(deleted, 3);
    assert_eq!(
        storage.find_last(&other()).await.unwrap(),
        Some(header(3, 0))
    );
}

/// Alice is paid "r" and rewarded in 5 of branch 0; branch 1 takes 5 and 6
/// back: they are deleted with r, its legs and the reward, and branch 1's
/// 5 to 7 link onto 4.
#[tokio::test]
async fn a_reorg_deletes_its_blocks_with_their_facts() {
    let (storage, db, directory) = seeded().await;
    let network = network();
    let alice = address(&network, "alice");
    watch(&storage, &alice, 0).await;
    let mut forked: Vec<Block> = (0..=6)
        .map(|position| block(&network, position, 0, vec![]))
        .collect();
    forked[5] = block(
        &network,
        5,
        0,
        vec![payment(&network, "r", "alice", 4, None)],
    );
    forked[5].rewards = vec![Reward {
        index: 0,
        address: alice.clone(),
        asset: native(&network),
        amount: Decimal::from(1),
    }];
    index(&storage, &forked).await;
    let legs = || db.read(|connection| Ok(crate::models::leg::db::count(connection)?));
    assert_eq!(legs().await.unwrap(), 2, "r's leg and the reward");

    let mut chain = forked[..5].to_vec();
    chain.extend((5..=7).map(|position| block(&network, position, 1, vec![])));
    chain[5].header.parent = Some(BlockParent {
        position: 4,
        hash: header(4, 0).hash,
    });
    index(&storage, &chain).await;
    assert_eq!(rows(&directory, "r"), (vec![], vec![]));
    assert_eq!(legs().await.unwrap(), 0);
    let kept: Vec<(i64, Option<i64>)> = (0..=7)
        .map(|position: i64| (position, (position > 0).then(|| position - 1)))
        .collect();
    assert_eq!(blocks(&directory), kept);
    assert_eq!(indexed_at(&storage, &alice).await, Some(7));
}

#[tokio::test]
async fn a_new_index_over_a_pruned_range_gets_the_same_legs() {
    let network = network();
    let mut seen = Vec::new();
    for pruned in [false, true] {
        let (storage, db, directory) = seeded().await;
        prunable(&storage, &db).await;
        if pruned {
            prune(&db).await;
        }
        // Erin from 4: pruned with 3 below it, so the run writes 4 again
        // with no parent.
        watch(&storage, &address(&network, "erin"), 4).await;
        index(&storage, &pruned_chain(&network)).await;
        if pruned {
            let kept = blocks(&directory);
            assert!(kept.contains(&(4, None)), "{kept:?}");
            assert!(kept.contains(&(5, Some(4))), "{kept:?}");
        }
        seen.push(reads(&db, &["alice", "bob", "dave", "erin"]).await);
    }
    assert_eq!(seen[0], seen[1]);
    let erin = &seen[1].0[3];
    assert_eq!(erin.1.as_array().map(Vec::len), Some(2), "{erin:?}");
    assert_eq!(
        erin.0.as_ref().unwrap().balance_pending.as_deref(),
        Some("4")
    );
}

/// Each of `names`' index state and mined legs, latest first, as
/// (position, kind, amount, total).
async fn opened(
    db: &Db,
    names: &'static [&str],
) -> Vec<(
    crate::models::wallet_index::IndexState,
    Vec<(i64, i64, String, String)>,
)> {
    let (read, _) = reads(db, names).await;
    read.into_iter()
        .map(|(index, legs)| {
            let legs = legs.as_array().unwrap().iter().map(|leg| {
                let text = |field: &str| leg[field].as_str().unwrap().to_owned();
                let number = |field: &str| leg[field].as_i64().unwrap();
                (
                    number("position"),
                    number("kind"),
                    text("amount"),
                    text("total"),
                )
            });
            (index.unwrap().state, legs.collect())
        })
        .collect()
}

#[tokio::test]
async fn a_watched_address_opens_at_its_balance_and_a_new_one_at_zero() {
    use crate::models::wallet_index::IndexState::Indexing;
    let (storage, db, _directory) = seeded().await;
    let network = network();
    let held = Arc::new(Held::new(network.clone()));
    held.set("watched", Some((3, 5)));
    held.set("generated", Some((3, 0)));
    let indexer = settling(&storage, Served::new(quiet(&network, 6), FINALITY), held);
    for name in ["watched", "generated"] {
        watch_at(&storage, &indexer, name, 2).await;
    }
    idle(&indexer).await;
    let adjustment = |amount: &str| vec![(3, 6, amount.to_owned(), amount.to_owned())];
    assert_eq!(
        opened(&db, &["watched", "generated"]).await,
        [(Indexing, adjustment("5")), (Indexing, vec![])]
    );
    let (read, _) = reads(&db, &["watched"]).await;
    assert_eq!(
        read[0].1[0]["id"],
        "bitcoin.regtest.watched.native.adjustment.3.0"
    );
    assert_eq!(read[0].1[0]["transaction"], serde_json::Value::Null);
}

/// `name`'s native index watched from `from` on `indexer`'s chain, kept in
/// `storage`.
pub(super) async fn watch_at(storage: &Stores, indexer: &Indexer, name: &str, from: u64) {
    let network = network();
    let target = WalletIndexTarget::new(indexer.chain(), address(&network, name), native(&network));
    let target = target.with_birthday(Birthday::At(from));
    WalletIndexStore::insert(storage, &target).await.unwrap();
}

/// alice, watched from 2 on a chain with balances at 20, final through 15:
/// she starts at 15, her balance unknown. The first step reads her balance
/// at a final block (9 at 16, finality one further on) and, in the same
/// step, reads on from 17: 5 paid at 4 is in the adjustment, never a leg;
/// 3 paid at 18 is one.
#[tokio::test]
async fn a_new_watch_starts_at_the_final_block_settles_and_reads_on() {
    use crate::models::wallet_index::IndexState::Indexing;
    let (storage, db, _directory) = seeded().await;
    let network = network();
    let mut chain = quiet(&network, 20);
    for (position, txid, amount) in [(4, "a", 5), (18, "b", 3)] {
        let paid = payment(&network, txid, "alice", amount, None);
        chain[position] = block(&network, u64::try_from(position).unwrap(), 0, vec![paid]);
    }
    let held = Arc::new(Held::new(network.clone()));
    held.set("alice", Some((16, 9)));
    let indexer = settling(&storage, Served::new(chain, 5), held);
    watch_at(&storage, &indexer, "alice", 2).await;
    assert_eq!(
        progress(&storage).await,
        [("alice".to_owned(), 2, 15, None)]
    );
    let (read, _) = reads(&db, &["alice"]).await;
    assert_eq!(read[0].0.as_ref().unwrap().balance_pending, None);

    assert_eq!(indexer.next().await.unwrap(), Next::Indexed(header(20, 0)));
    assert_eq!(
        progress(&storage).await,
        [("alice".to_owned(), 2, 16, Some(20))]
    );
    let legs = vec![leg(18, 0, "3", "12"), leg(16, 6, "9", "9")];
    assert_eq!(opened(&db, &["alice"]).await, [(Indexing, legs)]);
}

/// Solana now: the final block is the tip, the line 10 below it, with
/// balances. alice, watched from 2 at tip 30, starts below the line (19)
/// and settles once (9 at 30). At tip 35 the final blocks 31..=35 are read:
/// 3 paid at 33 is a leg of its transaction, and no settle runs (a balance
/// read would answer 100 at 35). Left behind (tip 55, line 45), she settles
/// by one balance read: 20 at 55, 1 paid at 40 inside the gap never read.
#[tokio::test]
async fn a_depth_line_with_final_at_the_tip_reads_blocks_and_settles_only_far_behind() {
    use crate::models::wallet_index::IndexState::Indexing;
    let (storage, db, _directory) = seeded().await;
    let network = network();
    let mut chain = quiet(&network, 55);
    for (position, txid, amount) in [(33, "b", 3), (40, "c", 1)] {
        let paid = payment(&network, txid, "alice", amount, None);
        chain[position] = block(&network, u64::try_from(position).unwrap(), 0, vec![paid]);
    }
    let held = Arc::new(Held::new(network.clone()));
    let at = |tip: usize| {
        let served = Served::new(chain[..=tip].to_vec(), 0);
        testing::indexer(
            served.with_line(Line::Depth(10)),
            Arc::new(storage.clone()),
            Some(held.clone()),
            None,
        )
    };
    held.set("alice", Some((30, 9)));
    let indexer = at(30);
    let target = WalletIndexTarget::new(
        indexer.chain(),
        address(&network, "alice"),
        native(&network),
    );
    let target = target.with_birthday(Birthday::At(2)).with_settle(true);
    WalletIndexStore::insert(&storage, &target).await.unwrap();
    assert_eq!(
        progress(&storage).await,
        [("alice".to_owned(), 2, 19, None)]
    );
    idle(&indexer).await;
    assert_eq!(
        progress(&storage).await,
        [("alice".to_owned(), 2, 30, Some(30))]
    );

    held.set("alice", Some((35, 100)));
    idle(&at(35)).await;
    assert_eq!(
        progress(&storage).await,
        [("alice".to_owned(), 2, 30, Some(35))]
    );
    let read = vec![leg(33, 0, "3", "12"), leg(30, 6, "9", "9")];
    assert_eq!(opened(&db, &["alice"]).await, [(Indexing, read.clone())]);
    let (legs, _) = reads(&db, &["alice"]).await;
    assert_eq!(legs[0].1[0]["transaction"], "bitcoin.regtest.b");

    held.set("alice", Some((55, 20)));
    let indexer = at(55);
    idle(&indexer).await;
    assert_eq!(
        progress(&storage).await,
        [("alice".to_owned(), 2, 55, Some(55))]
    );
    let settled = [vec![leg(55, 6, "8", "20")], read].concat();
    assert_eq!(opened(&db, &["alice"]).await, [(Indexing, settled)]);
    assert_eq!(indexer.stats().failures, 0);
}

/// alice (paid 5 at 50) and bob are indexed through 100 when the server
/// stops; alice has a send pending. Back at 4,100, final through 4,090,
/// both are behind the final block: the step checks the last kept block,
/// settles each at 4,090 (alice's chain balance 7, 2 paid inside the gap:
/// one adjustment of 2) and reads on from 4,091. The gap is never read; her
/// legs before it stay, and so does her send: its own index settles it
/// (design 085).
#[tokio::test]
async fn indexes_behind_the_final_block_settle_at_the_chains_balance_and_read_on() {
    use crate::models::wallet_index::IndexState::Indexing;
    let (storage, db, directory) = seeded().await;
    let network = network();
    // An account chain's legs carry no coin.
    let mut chain = quiet(&network, 4_100);
    for (position, txid, amount) in [(50, "f", 5_u64), (2_000, "g", 2)] {
        let paid = mined(
            &network,
            txid,
            0,
            vec![Leg::Received {
                address: address(&network, "alice"),
                asset: native(&network),
                amount: Decimal::from(amount),
                coin: None,
            }],
        );
        chain[position] = block(&network, u64::try_from(position).unwrap(), 0, vec![paid]);
    }
    for name in ["alice", "bob"] {
        watch(&storage, &address(&network, name), 0).await;
    }
    index(&storage, &chain[..=100]).await;
    confirm(&db, 100).await;
    let mut spend = pending(&network, "s", "carol", 5);
    spend.legs.push(Leg::Spent {
        address: address(&network, "alice"),
        asset: native(&network),
        amount: Decimal::from(5_u64),
        coin: None,
    });
    record(&db, &spend).await;
    assert_eq!(rows(&directory, "s"), (vec![None], vec![None]));

    let held = Arc::new(Held::new(network.clone()));
    held.set("alice", Some((4_090, 7)));
    held.set("bob", Some((4_090, 0)));
    let served = Served::new(chain, FINALITY);
    let indexer = settling(&storage, served.clone(), held);
    assert_eq!(
        indexer.next().await.unwrap(),
        Next::Indexed(header(4_100, 0))
    );
    let expected: BTreeSet<u64> = std::iter::once(100).chain(4_091..=4_100).collect();
    assert_eq!(served.reads(), expected, "the last kept block, S + 1 on");
    assert_eq!(
        progress(&storage).await,
        [
            ("alice".to_owned(), 0, 4_090, Some(4_100)),
            ("bob".to_owned(), 0, 4_090, Some(4_100)),
        ]
    );
    let alice = vec![leg(4_090, 6, "2", "7"), leg(50, 0, "5", "5")];
    let bob = vec![];
    assert_eq!(
        opened(&db, &["alice", "bob"]).await,
        [(Indexing, alice), (Indexing, bob)]
    );
    assert_eq!(
        rows(&directory, "s"),
        (vec![None], vec![None]),
        "the send stays pending"
    );
}

/// An adjustment moves the total only (design 083 §7). alice, on an
/// account chain, received 30 at 50 and sent 10 at 60, indexed through
/// 100; back at 4,100, final through 4,090, the chain reads 25: the settle
/// is +5, total 25, received 30 and sent 10 as before. 1 paid at 4,095
/// makes the total 26, received 31.
#[tokio::test]
async fn an_adjustment_moves_the_total_but_not_what_was_received_or_sent() {
    let (storage, db, _directory) = seeded().await;
    let network = network();
    let (alice, asset) = (address(&network, "alice"), native(&network));
    let received = |amount: u64| Leg::Received {
        address: alice.clone(),
        asset: asset.clone(),
        amount: Decimal::from(amount),
        coin: None,
    };
    let spent = Leg::Spent {
        address: alice.clone(),
        asset: asset.clone(),
        amount: Decimal::from(10_u64),
        coin: None,
    };
    let mut chain = quiet(&network, 4_100);
    for (position, txid, leg) in [
        (50, "f", received(30)),
        (60, "g", spent),
        (4_095, "h", received(1)),
    ] {
        let moved = mined(&network, txid, 0, vec![leg]);
        chain[position] = block(&network, u64::try_from(position).unwrap(), 0, vec![moved]);
    }
    watch(&storage, &alice, 0).await;
    index(&storage, &chain[..=100]).await;
    let held = Arc::new(Held::new(network.clone()));
    held.set("alice", Some((4_090, 25)));
    idle(&settling(&storage, Served::new(chain, FINALITY), held)).await;

    let (read, _) = reads(&db, &["alice"]).await;
    let field = |leg: &serde_json::Value, name: &str| leg[name].as_str().unwrap().to_owned();
    let legs: Vec<[String; 4]> = read[0]
        .1
        .as_array()
        .unwrap()
        .iter()
        .map(|leg| ["amount", "total", "total_received", "total_sent"].map(|name| field(leg, name)))
        .collect();
    let row = |values: [&str; 4]| values.map(str::to_owned);
    assert_eq!(
        legs,
        [
            row(["1", "26", "31", "10"]),
            row(["5", "25", "30", "10"]),
            row(["10", "20", "30", "10"]),
            row(["30", "30", "30", "0"]),
        ]
    );
    assert_eq!(read[0].1[1]["kind"], 6, "the settle");
}

/// Watched at 20, final through 15: alice's balance read answers nothing
/// yet, bob's fails, carol's answers. The step settles carol and reads on;
/// alice and bob stay as they were, unread by that run, one failure
/// counted. Once the chain answers, the next step settles them.
#[tokio::test]
async fn a_balance_read_not_answered_or_failed_leaves_the_index_for_the_next_step() {
    use crate::models::wallet_index::IndexState::Indexing;
    let (storage, db, _directory) = seeded().await;
    let network = network();
    let held = Arc::new(Held::new(network.clone()));
    held.set("alice", None);
    held.set("carol", Some((15, 1)));
    let indexer = settling(&storage, Served::new(quiet(&network, 20), 5), held.clone());
    for name in ["alice", "bob", "carol"] {
        watch_at(&storage, &indexer, name, 0).await;
    }
    assert_eq!(indexer.next().await.unwrap(), Next::Indexed(header(20, 0)));
    assert_eq!(
        progress(&storage).await,
        [
            ("alice".to_owned(), 0, 15, None),
            ("bob".to_owned(), 0, 15, None),
            ("carol".to_owned(), 0, 15, Some(20)),
        ]
    );
    assert_eq!(
        opened(&db, &["alice", "bob"]).await,
        [(Indexing, vec![]), (Indexing, vec![])]
    );
    let stats = indexer.stats();
    assert_eq!(stats.failures, 1, "{stats:?}");
    let error = stats.error.unwrap_or_default();
    assert!(error.starts_with("wallet index bob "), "{error}");
    assert!(
        error.contains("the node cannot read this balance"),
        "{error}"
    );

    held.set("alice", Some((15, 5)));
    held.set("bob", Some((15, 0)));
    assert_eq!(indexer.next().await.unwrap(), Next::Indexed(header(20, 0)));
    assert_eq!(
        progress(&storage).await,
        [
            ("alice".to_owned(), 0, 15, Some(20)),
            ("bob".to_owned(), 0, 15, Some(20)),
            ("carol".to_owned(), 0, 15, Some(20)),
        ]
    );
    assert_eq!(
        opened(&db, &["alice", "bob"]).await,
        [(Indexing, vec![leg(15, 6, "5", "5")]), (Indexing, vec![])]
    );
}

/// alice settles at 15 (final through 15 of 20) and the same step reads on
/// through 20, past the final block. At 22, final through 17, the next
/// steps read on from 21 and do not settle her again.
#[tokio::test]
async fn a_settled_index_reads_on_in_the_same_step_and_is_not_settled_again() {
    use crate::models::wallet_index::IndexState::Indexing;
    let (storage, db, _directory) = seeded().await;
    let network = network();
    let chain = quiet(&network, 22);
    let held = Arc::new(Held::new(network.clone()));
    held.set("alice", Some((15, 5)));
    let first = settling(
        &storage,
        Served::new(chain[..=20].to_vec(), 5),
        held.clone(),
    );
    watch_at(&storage, &first, "alice", 0).await;
    assert_eq!(first.next().await.unwrap(), Next::Indexed(header(20, 0)));
    assert_eq!(
        progress(&storage).await,
        [("alice".to_owned(), 0, 15, Some(20))]
    );

    // A settle now would read 9.
    held.set("alice", Some((17, 9)));
    idle(&settling(&storage, Served::new(chain, 5), held)).await;
    assert_eq!(
        progress(&storage).await,
        [("alice".to_owned(), 0, 15, Some(22))]
    );
    assert_eq!(
        opened(&db, &["alice"]).await,
        [(Indexing, vec![leg(15, 6, "5", "5")])]
    );
}

/// bob is read through 20, settled at 15. At 22, final through 17, alice is
/// watched: she starts at 17 and settles there, and the step's run starts
/// from her next, 18, not from the last kept block, 20.
#[tokio::test]
async fn the_run_starts_from_an_index_just_settled() {
    use crate::models::wallet_index::IndexState::Indexing;
    let (storage, db, _directory) = seeded().await;
    let network = network();
    let chain = quiet(&network, 22);
    let held = Arc::new(Held::new(network.clone()));
    held.set("bob", Some((15, 0)));
    held.set("alice", Some((17, 3)));
    let first = settling(
        &storage,
        Served::new(chain[..=20].to_vec(), 5),
        held.clone(),
    );
    watch_at(&storage, &first, "bob", 0).await;
    idle(&first).await;

    let served = Served::new(chain, 5);
    let second = settling(&storage, served.clone(), held);
    watch_at(&storage, &second, "alice", 0).await;
    assert_eq!(second.next().await.unwrap(), Next::Indexed(header(22, 0)));
    assert_eq!(
        served.reads(),
        (18..=22).collect(),
        "S + 1 on, S never read"
    );
    assert_eq!(
        progress(&storage).await,
        [
            ("alice".to_owned(), 0, 17, Some(22)),
            ("bob".to_owned(), 0, 15, Some(22)),
        ]
    );
    assert_eq!(
        opened(&db, &["alice"]).await,
        [(Indexing, vec![leg(17, 6, "3", "3")])]
    );
}

/// bob reads blocks 0 to 20; alice then settles at 15 (final through 15 of
/// 20, the chain reading 5) and is read through 20. Another branch from 15
/// then takes her adjustment's position back, past finality (final through
/// 19): the steps walk back to 14, the reorg's write deletes her adjustment
/// (it has no block to go with), her index rewinds behind its birthday, and
/// she settles again on the new branch, at 19 (6 there); bob, rewound behind
/// the line, settles there too.
#[tokio::test]
async fn a_reorg_across_a_settle_deletes_its_adjustment_and_settles_again() {
    use crate::models::wallet_index::IndexState::Indexing;
    let (storage, db, directory) = seeded().await;
    let network = network();
    watch(&storage, &address(&network, "bob"), 0).await;
    index(&storage, &quiet(&network, 20)).await;
    let held = Arc::new(Held::new(network.clone()));
    held.set("alice", Some((15, 5)));
    let first = settling(&storage, Served::new(quiet(&network, 20), 5), held.clone());
    watch_at(&storage, &first, "alice", 0).await;
    idle(&first).await;
    assert_eq!(
        opened(&db, &["alice"]).await,
        [(Indexing, vec![leg(15, 6, "5", "5")])]
    );

    let mut forked: Vec<Block> = (0..=24)
        .map(|p| block(&network, p, u8::from(p >= 15), vec![]))
        .collect();
    forked[15].header.parent = header(15, 0).parent;
    held.set("alice", Some((19, 6)));
    held.set("bob", Some((19, 0)));
    let second = settling(&storage, Served::new(forked, 5), held);
    idle(&second).await;
    assert_eq!(second.stats().reorgs, 6, "20 down to 15");
    assert_eq!(
        progress(&storage).await,
        [
            ("alice".to_owned(), 0, 19, Some(24)),
            ("bob".to_owned(), 0, 19, Some(24)),
        ]
    );
    assert_eq!(
        opened(&db, &["alice"]).await,
        [(Indexing, vec![leg(19, 6, "6", "6")])]
    );
    let at: Vec<i64> = adjustments(&directory).iter().map(|kept| kept.0).collect();
    assert_eq!(
        at,
        [19],
        "alice's at 15 is deleted, not only hidden; bob's at 19 is zero, not kept"
    );
}

/// alice settles at 15 and is read through 20: the blocks from 16 are kept,
/// not 15. Another branch from 16, above finality (final through 14), takes
/// them back: her index rewinds to 15, where she settled (no block there),
/// keeps her adjustment and reads the new branch on from 16, settled once.
#[tokio::test]
async fn a_reorg_above_a_settle_rewinds_its_index_to_the_settle() {
    use crate::models::wallet_index::IndexState::Indexing;
    let (storage, db, directory) = seeded().await;
    let network = network();
    let held = Arc::new(Held::new(network.clone()));
    held.set("alice", Some((15, 5)));
    let first = settling(&storage, Served::new(quiet(&network, 20), 5), held.clone());
    watch_at(&storage, &first, "alice", 0).await;
    idle(&first).await;

    let mut forked: Vec<Block> = (0..=24)
        .map(|p| block(&network, p, u8::from(p >= 16), vec![]))
        .collect();
    forked[16].header.parent = header(16, 0).parent;
    let second = settling(&storage, Served::new(forked, FINALITY), held);
    while second.next().await.unwrap() != Next::Reorged(16) {}
    assert_eq!(
        progress(&storage).await,
        [("alice".to_owned(), 0, 15, Some(15))]
    );
    idle(&second).await;
    assert_eq!(
        progress(&storage).await,
        [("alice".to_owned(), 0, 15, Some(24))]
    );
    assert_eq!(
        opened(&db, &["alice"]).await,
        [(Indexing, vec![leg(15, 6, "5", "5")])]
    );
    assert_eq!(adjustments(&directory), [(15, None)]);
}

/// The (position, block) of every adjustment kept, of any index.
fn adjustments(directory: &tempfile::TempDir) -> Vec<(i64, Option<String>)> {
    use diesel::{
        RunQueryDsl as _,
        sql_types::{BigInt, Nullable, Text},
    };
    #[derive(diesel::QueryableByName)]
    struct Kept {
        #[diesel(sql_type = BigInt)]
        position: i64,
        #[diesel(sql_type = Nullable<Text>)]
        block: Option<String>,
    }
    diesel::sql_query("SELECT position, block FROM leg WHERE kind = 6 ORDER BY position")
        .load::<Kept>(&mut raw(directory))
        .unwrap()
        .into_iter()
        .map(|kept| (kept.position, kept.block))
        .collect()
}

/// The store's settle keeps no block (design 087): alice settles at 10,
/// above the kept blocks 0 to 3, and bob at 2, a kept one; the adjustments
/// hold only positions and the blocks stay as they were. A second settle at
/// the same position answers `false`.
#[tokio::test]
async fn a_settle_keeps_no_block() {
    use crate::models::wallet_index::IndexState::Indexing;
    let (storage, db, directory) = seeded().await;
    let network = network();
    BlockStore::insert(&storage, &empty(&network, 0, 3, 0))
        .await
        .unwrap();
    for name in ["alice", "bob"] {
        watch(&storage, &address(&network, name), 0).await;
    }
    let all = WalletIndexStore::find_all(&storage, &network)
        .await
        .unwrap();
    let kept = blocks(&directory);
    let settle = async |index: &WalletIndex, position, amount: u64| {
        let held = Holding {
            position,
            amount: Decimal::from(amount),
        };
        WalletIndexStore::settle(&storage, index, &held).await
    };

    assert!(settle(&all[0], 10, 7).await.unwrap());
    assert!(settle(&all[1], 2, 1).await.unwrap());
    let alice = &WalletIndexStore::find_all(&storage, &network)
        .await
        .unwrap()[0];
    assert!(!settle(alice, 10, 8).await.unwrap());

    assert_eq!(blocks(&directory), kept);
    assert_eq!(adjustments(&directory), [(2, None), (10, None)]);
    assert_eq!(
        progress(&storage).await,
        [
            ("alice".to_owned(), 0, 10, Some(10)),
            ("bob".to_owned(), 0, 2, Some(2)),
        ]
    );
    assert_eq!(
        opened(&db, &["alice", "bob"]).await,
        [
            (Indexing, vec![leg(10, 6, "7", "7")]),
            (Indexing, vec![leg(2, 6, "1", "1")]),
        ]
    );
}

/// Each index of the test network: its address, requested start, birthday
/// and the block it is indexed through.
async fn progress(storage: &Stores) -> Vec<(String, u64, u64, Option<u64>)> {
    WalletIndexStore::find_all(storage, &network())
        .await
        .unwrap()
        .into_iter()
        .map(|index| {
            (
                index.address.value,
                index.requested_from,
                index.birthday,
                index.progress.indexed_at,
            )
        })
        .collect()
}

/// Sets the state of `name`'s index, as pausing or resuming it does.
async fn set_state(storage: &Stores, name: &str, state: IndexState) {
    let index = WalletIndexStore::find_all(storage, &network())
        .await
        .unwrap()
        .into_iter()
        .find(|index| index.address.value == name)
        .unwrap();
    WalletIndexStore::set_state(storage, &index, state, None)
        .await
        .unwrap();
}

/// alice is indexed through 100; meanwhile the chain took back 99 and 100
/// and reached 4,100, final through 4,090. On a chain with balances the
/// step first checks the last kept block below the final one: gone, it is
/// deleted, then 99; on 98, kept, she settles at 4,090 and reads on.
#[tokio::test]
async fn a_last_kept_block_the_chain_took_back_is_deleted_before_a_settle() {
    let (storage, _db, directory) = seeded().await;
    let network = network();
    watch(&storage, &address(&network, "alice"), 0).await;
    index(&storage, &quiet(&network, 100)).await;
    let mut chain: Vec<Block> = (0..=4_100)
        .map(|p| block(&network, p, u8::from(p >= 99), vec![]))
        .collect();
    chain[99].header.parent = header(99, 0).parent;
    let held = Arc::new(Held::new(network.clone()));
    held.set("alice", Some((4_090, 0)));
    let indexer = settling(&storage, Served::new(chain, FINALITY), held);
    assert_eq!(indexer.next().await.unwrap(), Next::Reorged(100));
    assert_eq!(indexer.next().await.unwrap(), Next::Reorged(99));
    assert_eq!(
        progress(&storage).await,
        [("alice".to_owned(), 0, 0, Some(98))]
    );
    assert_eq!(
        indexer.next().await.unwrap(),
        Next::Indexed(header(4_100, 1))
    );
    assert_eq!(
        progress(&storage).await,
        [("alice".to_owned(), 0, 4_090, Some(4_100))]
    );
    let kept = blocks(&directory);
    assert!(
        kept.iter().all(|row| ![99, 100].contains(&row.0)),
        "the reorg deleted 99 and 100"
    );
    assert!(
        kept.iter().all(|row| row.0 != 4_090),
        "no settle keeps a block"
    );
}

/// alice, paid 5 at 50, is indexed through 100; back at 4,100 (P = 4,000,
/// final through 4,090) the node serves 3,000 on only, not the last kept
/// block: it is final, and the step never reads the gap. With balances she
/// settles at 4,090 and reads on; without, she jumps to P and her coin is
/// written off at 100.
#[tokio::test]
async fn a_last_kept_block_the_node_no_longer_serves_counts_as_final() {
    use crate::models::wallet_index::IndexState::Indexing;
    let network = network();
    let held = Arc::new(Held::new(network.clone()));
    held.set("alice", Some((4_090, 7)));
    let a = coin(&network, "a", 0);
    for (balances, coin, from, read_from, mut legs) in [
        (
            Some(held),
            None,
            4_090,
            4_091,
            vec![leg(4_090, 6, "2", "7")],
        ),
        (None, Some(a), 4_000, 4_000, vec![leg(100, 6, "-5", "0")]),
    ] {
        let (storage, db, _directory) = seeded().await;
        let mut chain = quiet(&network, 4_100);
        let paid = Leg::Received {
            address: address(&network, "alice"),
            asset: native(&network),
            amount: Decimal::from(5_u64),
            coin,
        };
        chain[50] = block(&network, 50, 0, vec![mined(&network, "a", 0, vec![paid])]);
        watch(&storage, &address(&network, "alice"), 0).await;
        index(&storage, &chain[..=100]).await;
        let pruned = Served::new(chain, FINALITY);
        pruned.prune(3_000);
        let indexer = match balances {
            Some(held) => settling(&storage, pruned.clone(), held),
            None => jumping(&storage, pruned.clone()),
        };
        idle(&indexer).await;
        assert_eq!(pruned.reads(), (read_from..=4_100).collect(), "the line on");
        assert_eq!(
            progress(&storage).await,
            [("alice".to_owned(), 0, from, Some(4_100))]
        );
        legs.push(leg(50, 0, "5", "5"));
        assert_eq!(opened(&db, &["alice"]).await, [(Indexing, legs)]);
    }
}

/// alice is paused at 100 and bob at 245 of 250; both resumed at 250,
/// final through 240: alice, behind the final block, settles there and
/// reads on; bob, past it, reads on from 246.
#[tokio::test]
async fn a_resumed_index_settles_when_behind_the_final_block_and_reads_on_when_not() {
    let (storage, _db, _directory) = seeded().await;
    let network = network();
    let chain = quiet(&network, 250);
    for name in ["alice", "bob"] {
        watch(&storage, &address(&network, name), 0).await;
    }
    index(&storage, &chain[..=100]).await;
    set_state(&storage, "alice", IndexState::Paused).await;
    index(&storage, &chain[..=245]).await;
    set_state(&storage, "bob", IndexState::Paused).await;

    for name in ["alice", "bob"] {
        set_state(&storage, name, IndexState::Indexing).await;
    }
    let held = Arc::new(Held::new(network.clone()));
    held.set("alice", Some((240, 0)));
    let served = Served::new(chain, FINALITY);
    idle(&settling(&storage, served.clone(), held)).await;
    assert_eq!(served.reads(), (241..=250).collect());
    assert_eq!(
        progress(&storage).await,
        [
            ("alice".to_owned(), 0, 240, Some(250)),
            ("bob".to_owned(), 0, 0, Some(250)),
        ]
    );
}

/// Design 083 §10's example, in satoshis, on a chain without balances
/// (Bitcoin), tip 1,200: alice is paid 0.3 at 800 (coin a) and 0.2 at 840
/// (coin b); at 950 she pays bob 0.25 from coin a, 0.0499 back (coin s:1),
/// 0.0001 fee; at 1,050 she pays bob 0.1999 from coin b, 0.0001 fee.
fn archived_chain(network: &Network) -> Vec<Block> {
    let mut chain = quiet(network, 1_200);
    for (position, txid, amount) in [(800, "a", 30_000_000), (840, "b", 20_000_000)] {
        let paid = payment(network, txid, "alice", amount, None);
        chain[position] = block(network, u64::try_from(position).unwrap(), 0, vec![paid]);
    }
    let mut spend = send(
        network,
        "alice",
        "bob",
        29_990_000,
        10_000,
        coin(network, "a", 0),
    );
    spend.push(Leg::Received {
        address: address(network, "alice"),
        asset: native(network),
        amount: Decimal::from(4_990_000_u64),
        coin: Some(coin(network, "s", 1)),
    });
    chain[950] = block(network, 950, 0, vec![mined(network, "s", 0, spend)]);
    let spend = send(
        network,
        "alice",
        "bob",
        19_990_000,
        10_000,
        coin(network, "b", 0),
    );
    chain[1_050] = block(network, 1_050, 0, vec![mined(network, "t", 0, spend)]);
    chain
}

/// Indexes alice from 800 through 850 of [`archived_chain`]; then, carol
/// watched from 860 (not started), steps an indexer with `max_depth` 100
/// over it through 1,000 until idle: both jump to 900. The positions that
/// read.
pub(super) async fn jumped(storage: &Stores) -> BTreeSet<u64> {
    let network = network();
    let chain = archived_chain(&network);
    watch(storage, &address(&network, "alice"), 800).await;
    index(storage, &chain[..=850]).await;
    watch(storage, &address(&network, "carol"), 860).await;
    let served = Served::new(chain[..=1_000].to_vec(), FINALITY);
    idle(&jumping(storage, served.clone())).await;
    served.reads()
}

/// alice's received legs, latest first, as (position, `archived_at`), and
/// her spendable coins.
async fn archived(db: &Db) -> (Vec<(i64, Option<i64>)>, Vec<Coin>) {
    let (read, unspent) = reads(db, &["alice"]).await;
    let legs = read[0].1.as_array().unwrap();
    let received = legs.iter().filter(|leg| leg["kind"] == 0);
    let received = received.map(|leg| {
        (
            leg["position"].as_i64().unwrap(),
            leg["archived_at"].as_i64(),
        )
    });
    (
        received.collect(),
        unspent.into_iter().map(|input| input.coin).collect(),
    )
}

/// (position, kind, amount, total) of a leg, as [`opened`] reads it.
fn leg(position: i64, kind: i64, amount: &str, total: &str) -> (i64, i64, String, String) {
    (position, kind, amount.to_owned(), total.to_owned())
}

/// alice's legs through 1,000 of [`archived_chain`] after [`jumped`],
/// latest first: at 950 the spend of archived coin a, spent −0.2999, fee
/// −0.0001, change +0.0499, with no leg finding the coin: the total goes
/// negative, history is missing; at 850 the write-off of both coins.
fn jumped_legs() -> Vec<(i64, i64, String, String)> {
    vec![
        leg(950, 0, "4990000", "-25010000"),
        leg(950, 4, "10000", "-30000000"),
        leg(950, 1, "29990000", "-29990000"),
        leg(850, 6, "-50000000", "0"),
        leg(840, 0, "20000000", "50000000"),
        leg(800, 0, "30000000", "30000000"),
    ]
}

#[tokio::test]
async fn a_jump_without_balances_archives_coins_and_a_spend_of_one_goes_negative() {
    use crate::models::wallet_index::IndexState::Indexing;
    let (storage, db, _directory) = seeded().await;
    let network = network();
    let read = jumped(&storage).await;
    let expected: BTreeSet<u64> = std::iter::once(850).chain(900..=1_000).collect();
    assert_eq!(read, expected, "the last kept block, then P on");
    assert_eq!(
        progress(&storage).await,
        [
            ("alice".to_owned(), 800, 900, Some(1_000)),
            ("carol".to_owned(), 860, 900, Some(1_000)),
        ]
    );
    // carol never started: nothing written off.
    assert_eq!(
        opened(&db, &["alice", "carol"]).await,
        [(Indexing, jumped_legs()), (Indexing, vec![])]
    );
    let (received, unspent) = archived(&db).await;
    assert_eq!(received, [(950, None), (840, Some(900)), (800, Some(900))]);
    assert_eq!(unspent, [coin(&network, "s", 1)]);
}

#[tokio::test]
async fn a_reorg_takes_a_spend_of_an_archived_coin_back_and_the_coin_stays_archived() {
    use crate::models::wallet_index::IndexState::Indexing;
    let (storage, db, _directory) = seeded().await;
    let network = network();
    jumped(&storage).await;
    // Another branch from 950 takes the spend back.
    let chain = archived_chain(&network);
    let mut forked = chain[..=1_000].to_vec();
    for (position, kept) in forked.iter_mut().enumerate().skip(950) {
        *kept = block(&network, u64::try_from(position).unwrap(), 1, vec![]);
    }
    forked[950].header.parent = header(950, 0).parent;
    idle(&jumping(&storage, Served::new(forked, FINALITY))).await;
    let legs = jumped_legs()[3..].to_vec();
    assert_eq!(opened(&db, &["alice"]).await, [(Indexing, legs)]);
    let (received, unspent) = archived(&db).await;
    assert_eq!(received, [(840, Some(900)), (800, Some(900))]);
    assert!(unspent.is_empty());

    // Kept again, 950's spend subtracts again.
    let served = Served::new(chain[..=1_000].to_vec(), FINALITY);
    idle(&jumping(&storage, served)).await;
    assert_eq!(opened(&db, &["alice"]).await, [(Indexing, jumped_legs())]);
    assert_eq!(archived(&db).await.1, [coin(&network, "s", 1)]);
}

#[tokio::test]
async fn a_second_jump_writes_off_only_the_coins_not_archived_yet() {
    let (storage, db, _directory) = seeded().await;
    let network = network();
    jumped(&storage).await;
    // Through 1,050, where alice spends archived coin b: −0.2 more. Then at
    // 1,200, P = 1,100: only coin s:1 is written off, on 1,050.
    let chain = archived_chain(&network);
    let served = Served::new(chain[..=1_050].to_vec(), FINALITY);
    idle(&jumping(&storage, served)).await;
    idle(&jumping(&storage, Served::new(chain, FINALITY))).await;
    let mut legs = vec![
        leg(1_050, 6, "-4990000", "-50000000"),
        leg(1_050, 4, "10000", "-45010000"),
        leg(1_050, 1, "19990000", "-45000000"),
    ];
    legs.extend(jumped_legs());
    let kept = opened(&db, &["alice", "carol"]).await;
    assert_eq!((&kept[0].1, &kept[1].1), (&legs, &vec![]));
    let (received, unspent) = archived(&db).await;
    assert_eq!(
        received,
        [(950, Some(1_100)), (840, Some(900)), (800, Some(900))]
    );
    assert!(unspent.is_empty());
    let (read, _) = reads(&db, &["alice"]).await;
    assert_eq!(
        read[0].1[0]["id"],
        "bitcoin.regtest.alice.native.adjustment.1050.1"
    );
}

/// alice re-indexed after [`jumped`] (tip 1,000, P = 900): her index
/// forgotten, its legs with it, a start below P is refused and one at P
/// reads from there. Her coins from before the jump are gone: only the
/// change of 950's spend is spendable, and that spend of coin a, never
/// seen, takes her total negative.
#[tokio::test(flavor = "multi_thread")]
async fn a_re_index_after_a_jump_starts_at_the_depth_line_without_the_old_coins() {
    use crate::models::wallet_index::IndexState::Indexing;
    let (storage, db, _directory) = seeded().await;
    let network = network();
    jumped(&storage).await;
    db.write(|connection| {
        Ok(crate::models::wallet_index::db::delete(
            connection,
            "bitcoin.regtest.alice.native",
        )?)
    })
    .await
    .unwrap();
    // The scripted network, its tip read from the chain through 1,000.
    let chain = archived_chain(&network)[..=1_000].to_vec();
    let script = Script {
        chain: Some(Served::new(chain, FINALITY)),
        ..Script::default()
    };
    let container = limited(&db, Arc::new(script), Some(100));
    let wallet = crate::models::wallet::WalletId::of(&address(&network, "alice")).unwrap();
    let create = |start| {
        crate::usecase::wallets::create_index(&container, &db, &wallet, "native", Some(start))
    };
    let error = create(899).await.unwrap_err();
    assert!(error.message.contains("start at 900"), "{error:?}");
    assert!(create(900).await.unwrap().0);

    let served = Served::new(archived_chain(&network)[..=1_000].to_vec(), FINALITY);
    idle(&jumping(&storage, served)).await;
    assert_eq!(
        progress(&storage).await,
        [
            ("alice".to_owned(), 900, 900, Some(1_000)),
            ("carol".to_owned(), 860, 900, Some(1_000)),
        ]
    );
    assert_eq!(
        opened(&db, &["alice"]).await,
        [(Indexing, jumped_legs()[..3].to_vec())],
        "only what she read since"
    );
    assert_eq!(
        archived(&db).await,
        (vec![(950, None)], vec![coin(&network, "s", 1)])
    );
}

/// alice, indexed through 850 of [`archived_chain`], sent 950's transaction
/// s (spending coin a, change s:1) and it is still pending when the network
/// jumps to 900: the jump forgets it, its row and legs, and archives both
/// coins. Mined at 950, read after the jump, it comes back as any mined
/// transaction: its spend of archived coin a subtracts, the total goes
/// negative, coin a stays archived and the change is spendable.
#[tokio::test]
async fn a_pending_send_a_jump_forgot_comes_back_mined_and_spends_its_archived_coin() {
    use crate::models::wallet_index::IndexState::Indexing;
    let (storage, db, directory) = seeded().await;
    let network = network();
    let chain = archived_chain(&network);
    watch(&storage, &address(&network, "alice"), 800).await;
    index(&storage, &chain[..=850]).await;
    confirm(&db, 850).await;
    let mut sent = chain[950].transactions[0].clone();
    sent.state = TransactionState::Pending;
    sent.order = None;
    record(&db, &sent).await;
    assert_eq!(rows(&directory, "s"), (vec![None], vec![None; 3]));

    // The jump's step reads 900..=909 only: not 950 yet.
    let reading = jumping(&storage, Served::new(chain[..=1_000].to_vec(), FINALITY)).with_batch(10);
    reading.next().await.unwrap();
    assert_eq!(
        rows(&directory, "s"),
        (vec![], vec![]),
        "the jump forgets it"
    );
    let (received, unspent) = archived(&db).await;
    assert_eq!(received, [(840, Some(900)), (800, Some(900))]);
    assert!(unspent.is_empty());

    idle(&reading).await;
    let at = Some("bitcoin.regtest.03b600".to_owned());
    assert_eq!(rows(&directory, "s"), (vec![at.clone()], vec![at; 3]));
    assert_eq!(opened(&db, &["alice"]).await, [(Indexing, jumped_legs())]);
    let (received, unspent) = archived(&db).await;
    assert_eq!(received, [(950, None), (840, Some(900)), (800, Some(900))]);
    assert_eq!(unspent, [coin(&network, "s", 1)]);
}

/// alice and dave are indexed through 850 of [`archived_chain`], dave paid
/// 0.01 at 820 (coin d), when dave is paused and the network jumps alice to
/// 900. Resumed at 1,049 (P = 949) he is behind it: he jumps then, on his
/// own, coin d archived at 949 and written off on 850; alice's legs stay.
#[tokio::test]
async fn a_paused_index_resumed_behind_the_depth_line_jumps_on_its_own() {
    use crate::models::wallet_index::IndexState::Indexing;
    let (storage, db, _directory) = seeded().await;
    let network = network();
    let mut chain = archived_chain(&network);
    let paid = payment(&network, "d", "dave", 1_000_000, None);
    chain[820] = block(&network, 820, 0, vec![paid]);
    for name in ["alice", "dave"] {
        watch(&storage, &address(&network, name), 800).await;
    }
    index(&storage, &chain[..=850]).await;
    set_state(&storage, "dave", IndexState::Paused).await;
    let reading = |tip: usize| {
        let served = Served::new(chain[..=tip].to_vec(), FINALITY);
        jumping(&storage, served)
    };
    idle(&reading(1_000)).await;
    assert_eq!(
        progress(&storage).await,
        [
            ("alice".to_owned(), 800, 900, Some(1_000)),
            ("dave".to_owned(), 800, 800, Some(850)),
        ]
    );

    set_state(&storage, "dave", IndexState::Indexing).await;
    idle(&reading(1_049)).await;
    assert_eq!(
        progress(&storage).await,
        [
            ("alice".to_owned(), 800, 900, Some(1_049)),
            ("dave".to_owned(), 800, 949, Some(1_049)),
        ]
    );
    let dave = vec![
        leg(850, 6, "-1000000", "0"),
        leg(820, 0, "1000000", "1000000"),
    ];
    assert_eq!(
        opened(&db, &["alice", "dave"]).await,
        [(Indexing, jumped_legs()), (Indexing, dave)]
    );
    let (read, unspent) = reads(&db, &["dave"]).await;
    assert_eq!(read[0].1[1]["archived_at"], 949);
    assert!(unspent.is_empty());
    let (received, unspent) = archived(&db).await;
    assert_eq!(received, [(950, None), (840, Some(900)), (800, Some(900))]);
    assert_eq!(unspent, [coin(&network, "s", 1)]);
}

/// alice is indexed through 850 of [`archived_chain`] and paused; she
/// still sends 950's transaction s (coin a, change s:1), watched by its
/// transaction index from 900. The indexer, through 1,000, keeps it mined
/// at 950 with her legs, though she is paused.
async fn paused_send(storage: &Stores, db: &Db) -> Vec<Block> {
    use crate::models::wallet_index::IndexState::Paused;
    let network = network();
    let chain = archived_chain(&network);
    watch(storage, &address(&network, "alice"), 800).await;
    index(storage, &chain[..=850]).await;
    confirm(db, 850).await;
    set_state(storage, "alice", IndexState::Paused).await;
    let mut sent = chain[950].transactions[0].clone();
    sent.state = TransactionState::Pending;
    sent.order = None;
    record(db, &sent).await;
    look_for(storage, "s", 900, None).await;
    idle(&jumping(
        storage,
        Served::new(chain[..=1_000].to_vec(), FINALITY),
    ))
    .await;
    let legs = vec![
        leg(950, 0, "4990000", "24990000"),
        leg(950, 4, "10000", "20000000"),
        leg(950, 1, "29990000", "20010000"),
        leg(840, 0, "20000000", "50000000"),
        leg(800, 0, "30000000", "30000000"),
    ];
    assert_eq!(opened(db, &["alice"]).await, [(Paused, legs)]);
    let unspent = [coin(&network, "b", 0), coin(&network, "s", 1)];
    assert_eq!(archived(db).await.1, unspent);
    chain
}

/// After [`paused_send`], alice resumed at 1,000 (P = 900) jumps: coin b
/// archived and written off on 850, coin a spent and s:1 at 950 not (read
/// again). Reading 950 again writes her legs there again, after the
/// write-off: s counted once.
#[tokio::test]
async fn a_send_kept_while_paused_is_read_again_after_a_jump_and_counted_once() {
    use crate::models::wallet_index::IndexState::Indexing;
    let (storage, db, _directory) = seeded().await;
    let network = network();
    let chain = paused_send(&storage, &db).await;
    set_state(&storage, "alice", IndexState::Indexing).await;
    idle(&jumping(
        &storage,
        Served::new(chain[..=1_000].to_vec(), FINALITY),
    ))
    .await;
    let legs = vec![
        leg(950, 0, "4990000", "4990000"),
        leg(950, 4, "10000", "0"),
        leg(950, 1, "29990000", "10000"),
        leg(850, 6, "-20000000", "30000000"),
        leg(840, 0, "20000000", "50000000"),
        leg(800, 0, "30000000", "30000000"),
    ];
    assert_eq!(opened(&db, &["alice"]).await, [(Indexing, legs)]);
    let (received, unspent) = archived(&db).await;
    assert_eq!(received, [(950, None), (840, Some(900)), (800, None)]);
    assert_eq!(unspent, [coin(&network, "s", 1)]);
}

/// After [`paused_send`], alice resumed at 1,200 (P = 1,100) jumps past
/// 950: coin b and change s:1 archived and written off on 950, her last
/// leg, after s's spend: her total 0, s counted once.
#[tokio::test]
async fn a_send_kept_while_paused_below_the_jump_line_is_written_off_after() {
    use crate::models::wallet_index::IndexState::Indexing;
    let (storage, db, _directory) = seeded().await;
    let chain = paused_send(&storage, &db).await;
    set_state(&storage, "alice", IndexState::Indexing).await;
    idle(&jumping(&storage, Served::new(chain, FINALITY))).await;
    let legs = vec![
        leg(950, 6, "-24990000", "0"),
        leg(950, 0, "4990000", "24990000"),
        leg(950, 4, "10000", "20000000"),
        leg(950, 1, "29990000", "20010000"),
        leg(840, 0, "20000000", "50000000"),
        leg(800, 0, "30000000", "30000000"),
    ];
    assert_eq!(opened(&db, &["alice"]).await, [(Indexing, legs)]);
    let (received, unspent) = archived(&db).await;
    assert_eq!(
        received,
        [(950, Some(1_100)), (840, Some(1_100)), (800, None)]
    );
    assert!(unspent.is_empty());
}

/// alice, paid 5 at 50 (coin a) and 2 at 100 (coin b), is indexed through
/// 100; meanwhile the chain took back 99 and 100 and reached 4,100 (P =
/// 4,000). Without balances the jump's step first deletes 100 and 99,
/// coin b's leg with them, then writes coin a off on 98, the last live
/// block she is indexed through.
#[tokio::test]
async fn a_jump_without_balances_writes_off_below_a_last_kept_block_the_chain_took_back() {
    use crate::models::wallet_index::IndexState::Indexing;
    let (storage, db, directory) = seeded().await;
    let network = network();
    let mut kept = quiet(&network, 100);
    kept[50] = block(
        &network,
        50,
        0,
        vec![payment(&network, "a", "alice", 5, None)],
    );
    kept[100] = block(
        &network,
        100,
        0,
        vec![payment(&network, "b", "alice", 2, None)],
    );
    watch(&storage, &address(&network, "alice"), 0).await;
    index(&storage, &kept).await;
    let mut chain: Vec<Block> = (0..=4_100)
        .map(|p| block(&network, p, u8::from(p >= 99), vec![]))
        .collect();
    chain[99].header.parent = header(99, 0).parent;
    chain[50] = kept[50].clone();
    idle(&jumping(&storage, Served::new(chain, FINALITY))).await;
    assert_eq!(
        progress(&storage).await,
        [("alice".to_owned(), 0, 4_000, Some(4_100))]
    );
    let kept = blocks(&directory);
    assert!(
        kept.iter().all(|row| ![99, 100].contains(&row.0)),
        "the reorg deleted 99 and 100"
    );
    let legs = vec![leg(98, 6, "-5", "0"), leg(50, 0, "5", "5")];
    assert_eq!(opened(&db, &["alice"]).await, [(Indexing, legs)]);
    let (received, unspent) = archived(&db).await;
    assert_eq!(received, [(50, Some(4_000))]);
    assert!(unspent.is_empty());
}

/// After [`jumped`] the chain's tip is 1,100 (P = 1,000) when the prune job
/// runs, before the indexer reads on: below 1,000 only the blocks alice's
/// legs are on stay (800 and 840, the archived coins; 950, the spend; not
/// 850, the write-off's position, which holds no block), and 1,000, the
/// indexes' block; every leg stays. Read
/// on, 1,050 spends archived coin b: it subtracts, coin b stays archived,
/// only the change s:1 is spendable.
#[tokio::test]
async fn a_prune_after_a_jump_keeps_the_archived_coins_and_a_later_spend_of_one_subtracts() {
    use crate::models::wallet_index::IndexState::Indexing;
    let (storage, db, directory) = seeded().await;
    let network = network();
    jumped(&storage).await;
    db.write(|connection| {
        use diesel::connection::SimpleConnection as _;
        Ok(connection.batch_execute("UPDATE network SET max_depth = 100")?)
    })
    .await
    .unwrap();
    confirm(&db, 1_100).await;
    prune(&db).await;
    let kept: Vec<_> = [800, 840, 950, 1_000]
        .into_iter()
        .map(|position| (position, None))
        .collect();
    assert_eq!(blocks(&directory), kept);
    assert_eq!(opened(&db, &["alice"]).await, [(Indexing, jumped_legs())]);

    let served = Served::new(archived_chain(&network)[..=1_100].to_vec(), FINALITY);
    idle(&jumping(&storage, served)).await;
    let mut legs = vec![
        leg(1_050, 4, "10000", "-45010000"),
        leg(1_050, 1, "19990000", "-45000000"),
    ];
    legs.extend(jumped_legs());
    assert_eq!(opened(&db, &["alice"]).await, [(Indexing, legs)]);
    let (received, unspent) = archived(&db).await;
    assert_eq!(received, [(950, None), (840, Some(900)), (800, Some(900))]);
    assert_eq!(unspent, [coin(&network, "s", 1)]);
}

/// `txid` of the test network looked for from `from` (design 085),
/// expiring past height `expires` (none: never).
pub(super) async fn look_for(storage: &Stores, txid: &str, from: u64, expires: Option<u64>) {
    let index = base::TransactionIndex {
        transaction: TransactionId::new(network(), txid),
        broadcasted_at: from,
        expires_height: expires,
        found: None,
        reverted: false,
        progress: Progress::default(),
        confirmed: None,
    };
    base::TransactionIndexStore::insert(storage, &index)
        .await
        .unwrap();
}

/// Each transaction index of the test network: its txid, the position it
/// found its transaction at, the one it is indexed through, its state.
pub(super) async fn looked_for(
    storage: &Stores,
) -> Vec<(String, Option<u64>, Option<u64>, IndexState)> {
    base::TransactionIndexStore::find_all(storage, &network())
        .await
        .unwrap()
        .into_iter()
        .map(|index| {
            let progress = index.progress;
            (
                index.transaction.id,
                index.found,
                progress.indexed_at,
                progress.state,
            )
        })
        .collect()
}

/// s and t looked for from 2; s, paying dave (nobody's index), mined at
/// 4: s found at 4 and kept without a leg stays there, t reads on through
/// 6. Another branch from 4 takes it back: the reorg walk clears it and
/// rewinds both to 3; the branch mines it at 5, where it is found again
/// and stays, while t reads on.
#[tokio::test]
async fn a_transaction_index_reads_on_finds_its_transaction_and_a_reorg_clears_it() {
    use IndexState::Indexing;
    let (storage, _db, directory) = seeded().await;
    let network = network();
    look_for(&storage, "s", 2, None).await;
    look_for(&storage, "t", 2, None).await;
    let paid = || payment(&network, "s", "dave", 1, None);
    let mut chain = quiet(&network, 6);
    chain[4] = block(&network, 4, 0, vec![paid()]);
    index(&storage, &chain).await;
    assert_eq!(
        looked_for(&storage).await,
        [
            ("s".to_owned(), Some(4), Some(4), Indexing),
            ("t".to_owned(), None, Some(6), Indexing),
        ]
    );
    let at4 = Some("bitcoin.regtest.0400".to_owned());
    assert_eq!(rows(&directory, "s"), (vec![at4], vec![]), "kept, no leg");

    let mut forked: Vec<Block> = (0..=8)
        .map(|p| block(&network, p, u8::from(p >= 4), vec![]))
        .collect();
    forked[4].header.parent = header(4, 0).parent;
    forked[5] = block(&network, 5, 1, vec![paid()]);
    let indexer = indexer(&storage, Served::new(forked, FINALITY));
    while indexer.next().await.unwrap() != Next::Reorged(4) {}
    assert_eq!(
        looked_for(&storage).await,
        [
            ("s".to_owned(), None, Some(3), Indexing),
            ("t".to_owned(), None, Some(3), Indexing),
        ]
    );
    idle(&indexer).await;
    assert_eq!(
        looked_for(&storage).await,
        [
            ("s".to_owned(), Some(5), Some(5), Indexing),
            ("t".to_owned(), None, Some(8), Indexing),
        ]
    );
}

/// Alice, paid 5 at 0 (coin f), sends s spending it, looked for from 2; s
/// is mined at 2 and found there. A reorg from 2 takes it back: s is
/// pending again, its index unstarted, its leg too, so coin f stays held and alice can spend
/// nothing; the new branch mines it at 3, where it is found again.
#[tokio::test]
async fn our_send_in_a_reorged_block_goes_back_to_pending_and_is_found_again() {
    use IndexState::Indexing;
    let (storage, db, directory) = seeded().await;
    let network = network();
    let alice = address(&network, "alice");
    watch(&storage, &alice, 0).await;
    let funding = payment(&network, "f", "alice", 5, None);
    BlockStore::insert(
        &storage,
        &run(
            &network,
            vec![
                block(&network, 0, 0, vec![funding]),
                block(&network, 1, 0, vec![]),
            ],
        ),
    )
    .await
    .unwrap();
    confirm(&db, 0).await;
    let mut spend = pending(&network, "s", "bob", 5);
    spend.legs.push(Leg::Spent {
        address: alice.clone(),
        asset: native(&network),
        amount: Decimal::from(5_u64),
        coin: Some(coin(&network, "f", 0)),
    });
    record(&db, &spend).await;
    look_for(&storage, "s", 2, None).await;
    let mut mined = spend.clone();
    mined.state = TransactionState::Succeeded;
    mined.order = Some(0);
    let held = |storage: Stores, db: Db| {
        let alice = alice.clone();
        async move {
            let unspent = storage
                .find_unspent(std::slice::from_ref(&alice))
                .await
                .unwrap()
                .len();
            let spendable = db
                .read(|connection| {
                    crate::models::wallet_index::db::find_by_ids(
                        connection,
                        &super::support::scripted::network(),
                        &["bitcoin.regtest.alice.native".to_owned()],
                        0,
                    )
                })
                .await
                .unwrap()["bitcoin.regtest.alice.native"]
                .spendable()
                .unwrap();
            (unspent, spendable)
        }
    };
    assert_eq!(
        held(storage.clone(), db.clone()).await,
        (0, Decimal::zero())
    );

    BlockStore::insert(
        &storage,
        &run(&network, vec![block(&network, 2, 0, vec![mined.clone()])]),
    )
    .await
    .unwrap();
    assert_eq!(
        looked_for(&storage).await,
        [("s".to_owned(), Some(2), Some(2), Indexing)]
    );
    let at2 = Some("bitcoin.regtest.0200".to_owned());
    assert_eq!(rows(&directory, "s"), (vec![at2.clone()], vec![at2]));

    BlockStore::delete(&storage, &network, 2).await.unwrap();
    assert_eq!(rows(&directory, "s"), (vec![None], vec![None]), "pending");
    assert_eq!(
        looked_for(&storage).await,
        [("s".to_owned(), None, None, Indexing)]
    );
    assert_eq!(history(&directory, &alice), ["f", "s"]);
    assert_eq!(
        held(storage.clone(), db.clone()).await,
        (0, Decimal::zero())
    );

    let mut branch = vec![
        block(&network, 2, 1, vec![]),
        block(&network, 3, 1, vec![mined]),
    ];
    branch[0].header.parent = header(2, 0).parent;
    BlockStore::insert(&storage, &run(&network, branch))
        .await
        .unwrap();
    assert_eq!(
        looked_for(&storage).await,
        [("s".to_owned(), Some(3), Some(3), Indexing)]
    );
    let at3 = Some("bitcoin.regtest.0301".to_owned());
    assert_eq!(rows(&directory, "s"), (vec![at3.clone()], vec![at3]));
    assert_eq!(
        held(storage.clone(), db.clone()).await,
        (0, Decimal::zero())
    );
    assert_eq!(indexed_at(&storage, &alice).await, Some(3));
}

/// Alice is read through 6, a block whose parent is 4 (slot 5 skipped). A
/// reorg from 6 caps her at 5, where no block sits; the new branch's 6, its
/// parent 4 too, moves her on all the same, and its payment to her is kept.
#[tokio::test]
async fn an_index_capped_on_a_skipped_slot_reads_on() {
    let (storage, _db, directory) = seeded().await;
    let network = network();
    let alice = address(&network, "alice");
    watch(&storage, &alice, 0).await;
    let after_skip = |fork, transactions| {
        let mut six = block(&network, 6, fork, transactions);
        six.header.height = 5;
        six.header.parent = header(5, 0).parent;
        run(&network, vec![six])
    };
    BlockStore::insert(&storage, &empty(&network, 0, 4, 0))
        .await
        .unwrap();
    BlockStore::insert(&storage, &after_skip(0, vec![]))
        .await
        .unwrap();
    assert_eq!(indexed_at(&storage, &alice).await, Some(6));

    BlockStore::delete(&storage, &network, 6).await.unwrap();
    assert_eq!(indexed_at(&storage, &alice).await, Some(5));
    let paid = payment(&network, "p", "alice", 3, None);
    BlockStore::insert(&storage, &after_skip(1, vec![paid]))
        .await
        .unwrap();
    assert_eq!(indexed_at(&storage, &alice).await, Some(6));
    assert_eq!(history(&directory, &alice), ["p"]);
}

/// s and t looked for from 2, s mined reverted at 4, t succeeded at 5, the
/// network final through 3: each index is found at its position, reverted
/// or not by its transaction row's own outcome, and reads the network's final
/// block: neither is final yet. Final through 5, s is `Reverted`, t `Final`.
#[tokio::test]
async fn a_transaction_index_is_found_or_reverted_by_its_outcome() {
    use diesel::connection::SimpleConnection as _;
    let (storage, db, _directory) = seeded().await;
    let network = network();
    look_for(&storage, "s", 2, None).await;
    look_for(&storage, "t", 2, None).await;
    let mut reverted = payment(&network, "s", "dave", 1, None);
    reverted.state = TransactionState::Reverted(None);
    let mut chain = quiet(&network, 6);
    chain[4] = block(&network, 4, 0, vec![reverted]);
    chain[5] = block(
        &network,
        5,
        0,
        vec![payment(&network, "t", "dave", 1, None)],
    );
    index(&storage, &chain).await;
    let read = async |confirmed: u64| {
        let set = format!(
            "INSERT INTO network_index (chain, network, tip, confirmed) \
             VALUES ('bitcoin', 'regtest', {confirmed}, {confirmed}) \
             ON CONFLICT (chain, network) DO UPDATE SET confirmed = excluded.confirmed"
        );
        db.write(move |connection| Ok(connection.batch_execute(&set)?))
            .await
            .unwrap();
        base::TransactionIndexStore::find_all(&storage, &network)
            .await
            .unwrap()
            .into_iter()
            .map(|index| {
                let status = index.status();
                let state = index.progress.state;
                (
                    index.transaction.id,
                    index.found,
                    state,
                    index.confirmed,
                    status,
                )
            })
            .collect::<Vec<_>>()
    };
    use IndexState::Indexing;
    use base::TransactionStatus::{Final, Reverted, Unknown};
    assert_eq!(
        read(3).await,
        [
            (
                "s".to_owned(),
                Some(4),
                IndexState::Indexing,
                Some(3),
                Unknown
            ),
            ("t".to_owned(), Some(5), Indexing, Some(3), Unknown),
        ]
    );
    assert_eq!(
        read(5).await,
        [
            (
                "s".to_owned(),
                Some(4),
                IndexState::Indexing,
                Some(5),
                Reverted
            ),
            ("t".to_owned(), Some(5), Indexing, Some(5), Final),
        ]
    );
}

/// s looked for from 2 on Solana's terms, expiring after height 4, and t,
/// never expiring: the indexer reads both on past s's expiry, never
/// changing a state for it; expiry is s's broadcast job's. s is expired
/// once it is read through the confirmed position and the newest kept block
/// at or below it has a height above 4.
#[tokio::test]
async fn the_indexer_reads_a_transaction_index_on_past_its_expiry() {
    use IndexState::Indexing;
    let (storage, db, _directory) = seeded().await;
    let network = network();
    look_for(&storage, "s", 2, Some(4)).await;
    look_for(&storage, "t", 2, None).await;
    index(&storage, &quiet(&network, 7)).await;
    assert_eq!(
        looked_for(&storage).await,
        [
            ("s".to_owned(), None, Some(7), Indexing),
            ("t".to_owned(), None, Some(7), Indexing),
        ]
    );
    let expired = async |confirmed: u64| {
        confirm(&db, confirmed).await;
        let at = network.clone();
        let height = db
            .read(move |connection| {
                let confirmed = crate::models::network_index::db::find_confirmed(connection, &at)?
                    .expect("a tip");
                Ok(crate::models::block::db::find_height_at_or_below(
                    connection, &at, confirmed,
                )?)
            })
            .await
            .unwrap()
            .expect("a kept block");
        let s = base::TransactionIndexStore::find_all(&storage, &network).await;
        let s = s.unwrap().remove(0);
        assert_eq!(s.status(), base::TransactionStatus::Missing);
        s.expired(u64::try_from(height).unwrap())
    };
    assert!(!expired(4).await, "not past yet");
    assert!(expired(5).await);
    assert_eq!(looked_for(&storage).await[0].3, Indexing, "no state for it");
}

/// s is settled at 9, found at 5, over kept empty blocks 0 to 20: its
/// positions name no block, so pruning below 20 deletes them all but the
/// newest, and s stays as settled.
#[tokio::test]
async fn pruning_deletes_the_blocks_a_settled_transaction_index_reached() {
    let (storage, db, directory) = seeded().await;
    let network = network();
    BlockStore::insert(&storage, &empty(&network, 0, 20, 0))
        .await
        .unwrap();
    look_for(&storage, "s", 2, None).await;
    let s = base::TransactionIndexStore::find_all(&storage, &network)
        .await
        .unwrap()
        .remove(0);
    let confirmed = base::Confirmed {
        position: 9,
        found: Some(5),
        reverted: false,
    };
    base::TransactionIndexStore::settle(&storage, &s, &confirmed)
        .await
        .unwrap();
    let pruned = db
        .write(|connection| {
            Ok(crate::models::block::db::prune(
                connection,
                &super::support::scripted::network(),
                20,
            )?)
        })
        .await
        .unwrap();
    assert_eq!(pruned, 20);
    let kept: Vec<i64> = blocks(&directory).iter().map(|kept| kept.0).collect();
    assert_eq!(kept, [20]);
    assert_eq!(
        looked_for(&storage).await,
        [("s".to_owned(), Some(5), Some(9), IndexState::Indexing)]
    );
}

/// s, t and u looked for from 2 on a chain that settles, at 20, final
/// through 15: the chain finds s at 12, t nowhere through 15, and cannot
/// tell u yet. The step settles s and t at 15; t reads on through 20, s,
/// found, stays at 15; u stays as it is for the next step.
#[tokio::test]
async fn a_transaction_index_behind_the_final_block_settles_at_what_the_chain_confirms() {
    use IndexState::Indexing;
    let (storage, _db, directory) = seeded().await;
    let network = network();
    for txid in ["s", "t", "u"] {
        look_for(&storage, txid, 2, None).await;
    }
    let confirms = Arc::new(Confirms::new(network.clone()));
    confirms.set("s", Some((15, Some(12), false)));
    confirms.set("t", Some((15, None, false)));
    confirms.set("u", None);
    let served = Served::new(quiet(&network, 20), 5);
    let indexer = testing::indexer(
        served.with_line(Line::Final),
        Arc::new(storage.clone()),
        None,
        Some(confirms),
    );
    assert_eq!(indexer.next().await.unwrap(), Next::Indexed(header(20, 0)));
    assert_eq!(
        looked_for(&storage).await,
        [
            ("s".to_owned(), Some(12), Some(15), Indexing),
            ("t".to_owned(), None, Some(20), Indexing),
            ("u".to_owned(), None, None, Indexing),
        ]
    );
    let kept: Vec<i64> = blocks(&directory).iter().map(|kept| kept.0).collect();
    assert_eq!(kept, [16, 17, 18, 19, 20], "no settle keeps a block");
}

/// s, pending, is looked for from 850 when the network, without a chain to
/// confirm it, is at 1,000 with `max_depth` 100: behind the line, it is
/// given up (`no_data`, read no further) and s is forgotten (`jump`).
#[tokio::test]
async fn a_transaction_index_behind_the_depth_line_is_given_up_and_its_payment_forgotten() {
    let (storage, db, directory) = seeded().await;
    let network = network();
    record(&db, &pending(&network, "s", "bob", 1)).await;
    look_for(&storage, "s", 850, None).await;
    idle(&jumping(
        &storage,
        Served::new(quiet(&network, 1_000), FINALITY),
    ))
    .await;
    assert_eq!(
        looked_for(&storage).await,
        [("s".to_owned(), None, None, IndexState::NoData)]
    );
    let s = base::TransactionIndexStore::find_all(&storage, &network).await;
    assert_eq!(s.unwrap()[0].progress.reason.as_deref(), Some("jump"));
    assert_eq!(rows(&directory, "s"), (vec![], vec![]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_kept_leg_and_each_settle_are_logged_at_their_levels() {
    use tracing::{Instrument as _, instrument::WithSubscriber as _};

    let (storage, db, _directory) = seeded().await;
    let network = network();
    let alice = address(&network, "alice");
    watch(&storage, &alice, 0).await;
    let feed = crate::usecase::logs::feed();
    let mut heard = feed.subscribe();
    let dispatch = super::routes::subscriber(&db, feed);
    let paid = payment(&network, "a", "alice", 7, None);
    let blocks = run(
        &network,
        vec![
            block(&network, 0, 0, vec![]),
            block(&network, 1, 0, vec![paid]),
        ],
    );
    async {
        let span = crate::usecase::network_span(&network);
        let kept = BlockStore::insert(&storage, &blocks).instrument(span.clone());
        let kept = kept.await.unwrap();
        assert_eq!((kept.matched, kept.legs), (1, 1));
        let index = WalletIndexStore::find_all(&storage, &network)
            .await
            .unwrap();
        // The chain holds what the legs say, then 3 more: a gap.
        for (position, amount) in [(2, 7_u64), (3, 10)] {
            let held = Holding {
                position,
                amount: Decimal::from(amount),
            };
            let settled = WalletIndexStore::settle(&storage, &index[0], &held);
            assert!(settled.instrument(span.clone()).await.unwrap());
        }
    }
    .with_subscriber(dispatch)
    .await;

    let mut logs = Vec::new();
    while let Ok(log) = heard.try_recv() {
        logs.push(log);
    }
    let of = |target: &'static str| logs.iter().filter(move |log| log.target == target);
    let leg = of("index.leg").next().expect("an index.leg");
    assert_eq!(leg.level, "info");
    let fields = &leg.fields.0;
    let shown = ["wallet", "kind", "amount", "txid", "position", "total"]
        .map(|key| fields.get(key).map_or("", String::as_str));
    assert_eq!(shown, ["alice", "received", "7", "a", "1", "7"]);
    assert_eq!(leg.wallet_id.as_deref(), Some("bitcoin.regtest.alice"));
    assert_eq!(leg.transaction_id.as_deref(), Some("bitcoin.regtest.a"));
    let blocks: Vec<_> = of("index.block")
        .map(|log| (log.level.as_str(), log.fields.0["matched"].as_str()))
        .collect();
    assert_eq!(blocks, [("debug", "0"), ("info", "1")]);
    let settles: Vec<_> = of("index.settle")
        .map(|log| (log.level.as_str(), log.fields.0["adjustment"].as_str()))
        .collect();
    assert_eq!(settles, [("debug", "0"), ("warn", "3")], "{logs:?}");
    let gap = of("index.settle").next_back().unwrap();
    assert!(gap.fields.0.contains_key("why"), "{gap:?}");
    assert_eq!(gap.wallet_id.as_deref(), Some("bitcoin.regtest.alice"));
}
