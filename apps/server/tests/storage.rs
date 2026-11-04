//! The server's `models` and stores (`usecase`) on SQLite temp files: the `base`
//! store contracts, and the balance read. Offline and
//! deterministic.

use std::sync::{Arc, Mutex};

use base::{
    Address, Asset, Block, BlockHash, BlockHeader, BlockNotify, BlockParent, BlockStore, Blocks,
    Broadcaster, Builder, Chain, Coin, Decimal, Future, InputStore, Leg, Network, Networked,
    Reward, Transaction, TransactionError, TransactionId, TransactionState, Wallet,
    WalletFactories, WalletFactory, WalletFactoryBroadcaster, WalletIndex, WalletIndexStore,
    WalletStore,
};
use crypto::{Curve, SecretBytes};

use crate::{
    db::{Db, Page},
    usecase::sdk::Stores,
};

async fn storage() -> (Stores, Db, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let db = crate::db::Db::open(&directory.path().join("test.db")).unwrap();
    db.migrate().await.unwrap();
    (Stores::new(db.clone()), db, directory)
}

/// The signed balance of `address` in `asset` over every live block, as the
/// balance route reads it.
async fn balance(db: &Db, address: &Address, asset: &Asset) -> anyhow::Result<Decimal> {
    let id = crate::models::wallet_index::WalletIndex::id(address, asset)?;
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

fn network() -> Network {
    Network::new(Chain::new("bitcoin"), "regtest")
}

fn other() -> Network {
    Network::new(Chain::new("bitcoin"), "testnet")
}

fn address(network: &Network, name: &str) -> Address {
    Address::new(network.clone(), name)
}

fn native(network: &Network) -> Asset {
    Asset::native(network.clone())
}

/// Header `position` of branch `fork`; the parent is `position - 1` of the
/// same branch.
fn header(position: u64, fork: u8) -> BlockHeader {
    let hash = |position: u64| BlockHash(vec![u8::try_from(position).unwrap(), fork]);
    BlockHeader {
        position,
        height: position,
        hash: hash(position),
        parent: position.checked_sub(1).map(|parent| BlockParent {
            position: parent,
            hash: hash(parent),
        }),
        timestamp: None,
    }
}

fn coin(network: &Network, txid: &str, index: u32) -> Coin {
    Coin {
        transaction: TransactionId::new(network.clone(), txid),
        index,
    }
}

/// A transaction `txid` paying `amount` to `to` (coin `txid:0`) and, when
/// given, spending `spends` from `from`.
fn payment(
    network: &Network,
    txid: &str,
    to: &str,
    amount: u64,
    spends: Option<(&str, Coin)>,
) -> Transaction {
    let mut legs = vec![Leg::Received {
        address: address(network, to),
        asset: native(network),
        amount: Decimal::from(amount),
        coin: Some(coin(network, txid, 0)),
    }];
    if let Some((from, spent)) = spends {
        legs.push(Leg::Spent {
            address: address(network, from),
            asset: native(network),
            amount: Decimal::from(amount),
            coin: Some(spent),
        });
    }
    Transaction {
        id: TransactionId::new(network.clone(), txid),
        state: TransactionState::Succeeded,
        order: Some(0),
        legs,
        bytes: txid.as_bytes().to_vec(),
        expires: None,
    }
}

fn block(network: &Network, position: u64, fork: u8, transactions: Vec<Transaction>) -> Block {
    Block {
        network: network.clone(),
        header: header(position, fork),
        transactions: transactions.into(),
        rewards: Vec::new(),
    }
}

fn run(network: &Network, blocks: Vec<Block>) -> Blocks {
    let mut run = Blocks::new(network.clone());
    for block in blocks {
        assert!(run.push(block));
    }
    run
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

async fn watch(storage: &Stores, address: &Address, birthday: u64) {
    let index = WalletIndex {
        address: address.clone(),
        asset: native(&address.network),
        requested_from: birthday,
        birthday,
        indexed_at: None,
        state: base::WalletIndexState::Indexing,
        reason: None,
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
    index.indexed_at.map(|header| header.position)
}

/// The txids with a leg of `address`, pending or in a live block: mined ones
/// in chain order, then pending ones.
fn history(directory: &tempfile::TempDir, address: &Address) -> Vec<String> {
    use diesel::{Connection as _, RunQueryDsl as _, sql_types::Text};
    #[derive(diesel::QueryableByName)]
    struct Txid {
        #[diesel(sql_type = Text)]
        txid: String,
    }
    let path = directory.path().join("test.db");
    let mut connection = diesel::SqliteConnection::establish(path.to_str().unwrap()).unwrap();
    diesel::sql_query(
        r#"SELECT t.txid FROM "transaction" t LEFT JOIN block b ON b.id = t.block
        WHERE (t.block IS NULL OR b.archived_at IS NULL)
          AND EXISTS (SELECT 1 FROM leg l WHERE l."transaction" = t.id AND l.address = ?)
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
async fn insert_links_and_refuses_non_linking_blocks() {
    let (storage, _db, _directory) = storage().await;
    let network = network();
    BlockStore::insert(&storage, &empty(&network, 0, 3, 0))
        .await
        .unwrap();
    assert_eq!(
        storage.find_last(&network).await.unwrap(),
        Some(header(3, 0))
    );

    // A first new block whose parent is not the last kept one.
    let error = BlockStore::insert(&storage, &empty(&network, 4, 5, 1))
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("does not link"), "{error:#}");
    // A conflicting hash at a kept position; nothing of the run is written.
    let mixed = run(&network, vec![block(&network, 3, 1, vec![])]);
    assert!(BlockStore::insert(&storage, &mixed).await.is_err());
    assert_eq!(
        storage.find_last(&network).await.unwrap(),
        Some(header(3, 0))
    );

    BlockStore::insert(&storage, &empty(&network, 2, 5, 0))
        .await
        .unwrap();
    assert_eq!(
        storage.find_last(&network).await.unwrap(),
        Some(header(5, 0))
    );
}

#[tokio::test]
async fn insert_keeps_only_indexed_pairs_from_their_birthday() {
    let (storage, _db, directory) = storage().await;
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
    let (storage, _db, directory) = storage().await;
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
    assert_eq!(storage.find_next(&network, Some(2)).await.unwrap(), Some(1));
    // The indexer refetches from bob's birthday.
    let rerun = run(&network, blocks.iter().skip(1).cloned().collect());
    BlockStore::insert(&storage, &rerun).await.unwrap();
    let found = rows(&directory, "x");
    assert_eq!(found.0.len(), 1, "one transaction row");
    assert_eq!(found.1.len(), 2, "bob's leg added once");
    assert_eq!(history(&directory, &bob), ["x"]);
    assert_eq!(indexed_at(&storage, &bob).await, Some(2));
    assert_eq!(storage.find_next(&network, Some(2)).await.unwrap(), None);

    // A repeat is a no-op.
    BlockStore::insert(&storage, &rerun).await.unwrap();
    assert_eq!(rows(&directory, "x"), found);
}

#[tokio::test]
async fn a_new_index_below_the_kept_run_backfills_beneath_it() {
    let (storage, _db, directory) = storage().await;
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
    let (storage, _db, directory) = storage().await;
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
    let (storage, _db, _directory) = storage().await;
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
    let (storage, _db, _directory) = storage().await;
    let network = network();
    assert_eq!(storage.find_next(&network, None).await.unwrap(), None);
    watch(&storage, &address(&network, "a"), 5).await;
    watch(&storage, &address(&network, "b"), 2).await;
    assert_eq!(storage.find_next(&network, None).await.unwrap(), Some(2));
    assert_eq!(storage.find_next(&network, Some(1)).await.unwrap(), None);
    BlockStore::insert(&storage, &empty(&network, 2, 6, 0))
        .await
        .unwrap();
    assert_eq!(storage.find_next(&network, None).await.unwrap(), Some(7));
    assert_eq!(storage.find_next(&network, Some(6)).await.unwrap(), None);
}

#[tokio::test]
async fn notifying_storage_tells_listeners_after_commit() {
    let (storage, _db, _directory) = storage().await;
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
    let (storage, db, directory) = storage().await;
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
    let (storage, db, directory) = storage().await;
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

/// The `transaction` rows of `txid`: their block ids, and the block ids of
/// their legs.
fn rows(directory: &tempfile::TempDir, txid: &str) -> (Vec<Option<String>>, Vec<Option<String>>) {
    use diesel::{Connection as _, RunQueryDsl as _, sql_types::Text};
    let path = directory.path().join("test.db");
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

/// The indexer row at `tip`, every block confirmed: what spend checks read.
async fn confirm(db: &Db, tip: u64) {
    db.write(move |connection| {
        crate::models::network::db::save(connection, &network(), tip, tip, None, 0)
    })
    .await
    .unwrap();
}

/// Records `transaction` as pending, as the queued broadcaster does.
async fn record(db: &Db, transaction: &Transaction) {
    let transaction = transaction.clone();
    db.write(move |connection| {
        let mut pending = crate::models::transaction::Transaction::try_from(&transaction)?;
        pending.sent_at = Some(0);
        crate::models::transaction::db::save_pending(connection, &mut pending)
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_pending_spend_holds_its_coin_until_it_expires() {
    let (storage, db, _directory) = storage().await;
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
    spend.expires = Some(100);
    record(&db, &spend).await;
    assert_eq!(unspent(storage.clone()).await, 0);
    let expired = |now| {
        let (db, network) = (db.clone(), network.clone());
        async move {
            db.write(move |connection| {
                Ok(crate::models::transaction::db::expire(
                    connection, &network, now,
                )?)
            })
            .await
            .unwrap()
        }
    };
    assert_eq!(expired(99).await, 0);
    assert_eq!(unspent(storage.clone()).await, 0);
    assert_eq!(expired(100).await, 1);
    assert_eq!(unspent(storage.clone()).await, 1);
}

fn pending(network: &Network, txid: &str, to: &str, amount: u64) -> Transaction {
    let mut transaction = payment(network, txid, to, amount, None);
    transaction.state = TransactionState::Pending;
    transaction.order = None;
    transaction
}

#[tokio::test]
async fn mining_upserts_the_pending_row_and_its_legs() {
    let (storage, db, directory) = storage().await;
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
    let (storage, db, directory) = storage().await;
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

#[tokio::test]
async fn pending_legs_never_count() {
    let (storage, db, _directory) = storage().await;
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

/// A wallet whose key is its address text.
struct Keyed(Address);

impl Networked for Keyed {
    fn network(&self) -> &Network {
        &self.0.network
    }
}

impl base::Addressed for Keyed {
    fn address(&self) -> &Address {
        &self.0
    }
}

impl Wallet for Keyed {
    fn transaction(&self, _: &Asset) -> Result<Box<dyn Builder>, TransactionError> {
        Err(TransactionError::failed("unused"))
    }

    fn private_key(&self) -> (Curve, SecretBytes) {
        (Curve::Ed25519, SecretBytes::new(self.0.value.as_bytes()))
    }
}

struct Factory(Network);

impl WalletFactory for Factory {
    fn generate(&self) -> Future<'_, anyhow::Result<SecretBytes>> {
        Box::pin(async { anyhow::bail!("unused") })
    }

    fn from_pk(&self, secret: SecretBytes) -> Future<'_, anyhow::Result<Arc<dyn Wallet>>> {
        let text = String::from_utf8(secret.as_bytes().to_vec());
        let network = self.0.clone();
        Box::pin(
            async move { Ok(Arc::new(Keyed(Address::new(network, text?))) as Arc<dyn Wallet>) },
        )
    }

    fn from_pk_text(&self, text: &str) -> Future<'_, anyhow::Result<Arc<dyn Wallet>>> {
        self.from_pk(SecretBytes::new(text.as_bytes()))
    }

    fn parse_address(&self, text: &str) -> anyhow::Result<Address> {
        Ok(Address::new(self.0.clone(), text))
    }

    fn transaction(&self, _: &Asset) -> Result<Box<dyn Builder>, TransactionError> {
        Err(TransactionError::failed("unused"))
    }

    fn fees(&self) -> Arc<dyn base::FeeEstimator> {
        unimplemented!("these tests price no sends")
    }
}

struct Silent;

impl Broadcaster for Silent {
    fn broadcast<'a>(&'a self, _: &'a Transaction) -> Future<'a, Result<(), TransactionError>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn wallets_round_trip_through_their_factory() {
    let (storage, _db, _directory) = storage().await;
    let network = network();
    let mut factories = WalletFactories::new();
    factories.set(WalletFactoryBroadcaster {
        network: network.clone(),
        factory: Arc::new(Factory(network.clone())),
        broadcaster: Arc::new(Silent),
    });
    let alice = Keyed(address(&network, "alice"));
    WalletStore::insert(&storage, &alice).await.unwrap();
    WalletStore::insert(&storage, &Keyed(address(&network, "bob")))
        .await
        .unwrap();
    let restored = WalletStore::find_all(&storage, &factories).await.unwrap();
    let mut addresses: Vec<_> = restored.iter().map(|w| w.address().value.clone()).collect();
    addresses.sort();
    assert_eq!(addresses, ["alice", "bob"]);
}

#[tokio::test]
async fn rewards_of_indexed_pairs_are_kept_once() {
    let (storage, _db, _directory) = storage().await;
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
    let (storage, db, _directory) = storage().await;
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
fn raw(directory: &tempfile::TempDir) -> diesel::SqliteConnection {
    use diesel::{Connection as _, connection::SimpleConnection as _};
    let path = directory.path().join("test.db");
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
    let (storage, _db, directory) = storage().await;
    BlockStore::insert(&storage, &empty(&crate::tests::network(), 0, 0, 0))
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
    let (storage, _db, directory) = storage().await;
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
    assert_eq!(
        ids("SELECT indexed_at AS id FROM wallet_index"),
        ["bitcoin.regtest.0100"]
    );
    // A part with a dot is refused before anything is written.
    let dotted = WalletIndex {
        address: address(&network, "a.b"),
        asset: native(&network),
        requested_from: 0,
        birthday: 0,
        indexed_at: None,
        state: base::WalletIndexState::Indexing,
        reason: None,
    };
    assert!(WalletIndexStore::insert(&storage, &dotted).await.is_err());
}

/// A transaction `txid` at `order` in its block with `legs`.
fn mined(network: &Network, txid: &str, order: u64, legs: Vec<Leg>) -> Transaction {
    Transaction {
        id: TransactionId::new(network.clone(), txid),
        state: TransactionState::Succeeded,
        order: Some(order),
        legs,
        bytes: txid.as_bytes().to_vec(),
        expires: None,
    }
}

/// A send of `amount` from `from` to `to`, spending `coin`, paying `fee`.
fn send(network: &Network, from: &str, to: &str, amount: u64, fee: u64, spent: Coin) -> Vec<Leg> {
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
/// the confirmed balance less the pending outgoing legs.
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
        WHERE b.archived_at IS NULL AND l.asset = 'native'
        ORDER BY l.wallet_index_id, b.position, l."transaction" IS NULL, t."order", l.leg_index"#,
    )
    .load(&mut raw(directory))
    .unwrap();
    assert!(!rows.is_empty());
    // What each address has received and sent so far.
    let mut running = std::collections::BTreeMap::<String, (Decimal, Decimal)>::new();
    // (address, position, balance right after the leg), in order.
    let mut steps = Vec::new();
    for row in rows {
        assert_eq!(
            row.wallet_index_id,
            format!("bitcoin.regtest.{}.native", row.address)
        );
        let amount: Decimal = row.amount.parse().unwrap();
        let (received, sent) = running
            .entry(row.address.clone())
            .or_insert_with(|| (Decimal::zero(), Decimal::zero()));
        match row.kind {
            0 | 3 | 5 => *received += amount,
            _ => *sent += amount,
        }
        let sum = &*received - &*sent;
        let stored = [row.total, row.total_received, row.total_sent]
            .map(|text| text.parse::<Decimal>().unwrap());
        let expected = [sum, received.clone(), sent.clone()];
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
        WHERE l.position IS NULL AND l.asset = 'native' AND l.kind IN (1, 2, 4)",
    )
    .load(&mut raw(directory))
    .unwrap();
    let out = |name: &str| {
        pending
            .iter()
            .filter(|row| row.address == name)
            .fold(Decimal::zero(), |sum, row| {
                sum + row.amount.parse::<Decimal>().unwrap()
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
                let network = crate::tests::network();
                crate::models::network::db::save(connection, &network, tip, confirmed, None, 0)?;
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
            let read = [&index.balance, &index.balance_received, &index.balance_sent]
                .map(|text| parse(text.as_deref().unwrap()));
            assert_eq!(
                read,
                totals(&one.address, through),
                "{} at {through}",
                one.address
            );
            let tip = parse(&index.balance_pending);
            assert_eq!(
                tip,
                at(&one.address, i64::MAX),
                "{} at the tip",
                one.address
            );
            let spendable = parse(index.balance_spendable.as_deref().unwrap());
            let expected = at(&one.address, through) - out(&one.address);
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
    for through in 0..=tip {
        let ids = ids.clone();
        let balances = db
            .read(move |connection| {
                crate::models::wallet_index::db::find_by_ids(
                    connection,
                    &crate::tests::network(),
                    &ids,
                    through,
                )
            })
            .await
            .unwrap();
        for name in names {
            let id = format!("bitcoin.regtest.{name}.native");
            let confirmed = balances[&id]
                .balance
                .as_deref()
                .map(|total| total.parse::<Decimal>().unwrap());
            assert_eq!(confirmed, Some(at(name, through)), "{name} at {through}");
            let spendable = balances[&id].spendable().unwrap();
            assert_eq!(
                spendable,
                at(name, through) - out(name),
                "{name} at {through}"
            );
        }
    }
}

#[tokio::test]
async fn running_totals_match_the_amounts_through_reorgs_and_sends() {
    let (storage, db, directory) = storage().await;
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

    // A pending send, later mined with another fee; one that expires.
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
    pending_q.expires = Some(100);
    record(&db, &pending_q).await;
    // Pending legs: alice's sends lower her spendable only; bob's and
    // carol's incoming ones change nothing; no balance moves.
    totals_hold(&db, &directory, &names, 1).await;
    let spendable = |db: Db| async move {
        db.read(|connection| {
            crate::models::wallet_index::db::find_by_ids(
                connection,
                &crate::tests::network(),
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
    let expired = {
        let network = network.clone();
        db.write(move |connection| {
            Ok(crate::models::transaction::db::expire(
                connection, &network, 100,
            )?)
        })
        .await
        .unwrap()
    };
    assert_eq!(expired, 1);
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
    assert_eq!(spendable(db.clone()).await, Decimal::from(10));
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

/// A pending send of `amount` from alice to bob paying `fee`, expiring at
/// `expires`.
fn pending_send(
    network: &Network,
    txid: &str,
    amount: u64,
    fee: u64,
    expires: Option<u64>,
) -> Transaction {
    let spent = coin(network, "f", u32::try_from(amount).unwrap());
    let mut transaction = mined(
        network,
        txid,
        0,
        send(network, "alice", "bob", amount, fee, spent),
    );
    transaction.state = TransactionState::Pending;
    transaction.order = None;
    transaction.expires = expires;
    transaction
}

/// Two pending sends of one index: (a) both mined in one block, out of
/// list order; (b) mined in two blocks; (c) one mined, one expired. The
/// totals hold and spendable (total less pending outgoing) follows.
#[tokio::test]
async fn two_pending_sends_of_one_index_land_in_the_totals() {
    let network = network();
    let names = ["alice", "bob"];
    let alice = "bitcoin.regtest.alice.native";
    for case in ['a', 'b', 'c'] {
        let (storage, db, directory) = storage().await;
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
        record(&db, &pending_send(&network, "p1", 3, 1, None)).await;
        record(&db, &pending_send(&network, "p2", 5, 1, Some(100))).await;
        let spendable = |db: Db| async move {
            db.read(|connection| {
                crate::models::wallet_index::db::find_by_ids(
                    connection,
                    &crate::tests::network(),
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
        let p1 = mined(
            &network,
            "p1",
            1,
            pending_send(&network, "p1", 3, 2, None).legs,
        );
        let p2 = mined(
            &network,
            "p2",
            0,
            pending_send(&network, "p2", 5, 2, None).legs,
        );
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
                let network = network.clone();
                let expired = db
                    .write(move |connection| {
                        Ok(crate::models::transaction::db::expire(
                            connection, &network, 100,
                        )?)
                    })
                    .await
                    .unwrap();
                assert_eq!(expired, 1);
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
    assert_eq!(model.id, "bitcoin.regtest.0200");
    assert_eq!((model.position, model.parent.as_deref()), (2, None));
    let txids: Vec<_> = model.transactions.iter().map(|t| t.txid.as_str()).collect();
    assert_eq!(txids, ["a", "b"]);
    let b = &model.transactions[1];
    assert_eq!(
        (b.id.as_str(), b.block.as_deref()),
        ("bitcoin.regtest.b", Some(model.id.as_str()))
    );
    let legs: Vec<_> = b
        .legs
        .iter()
        .map(|leg| (leg.id.as_str(), leg.kind, leg.wallet_index_id.as_str()))
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
            (leg.block.as_deref(), leg.position),
            (Some(model.id.as_str()), Some(2))
        );
        assert_eq!((leg.seq, leg.total.as_deref()), (None, None));
    }
}

/// A pending transaction as a model: no block, no position; its expiry kept.
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
    sent.expires = Some(9);
    let model = crate::models::transaction::Transaction::try_from(&sent).unwrap();
    assert_eq!(
        (model.block, model.order, model.state, model.expires_at),
        (None, None, 0, Some(9))
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
    model.keep(&["bitcoin.regtest.alice.native".to_owned()].into());
    let kept: Vec<_> = model.transactions.iter().map(|t| t.txid.as_str()).collect();
    assert_eq!(kept, ["a"]);
    assert_eq!(model.transactions[0].legs.len(), 1);
    assert!(model.rewards.is_empty());
}

/// A block read plain, its transactions by order each with its legs, its
/// reward as a leg of its index; the mempool and one transaction likewise.
#[tokio::test]
async fn blocks_and_transactions_read_with_their_children() {
    let (storage, db, _directory) = storage().await;
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
            let network = crate::tests::network();
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
            let network = crate::tests::network();
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
async fn the_network_row_is_fresh_only_when_its_tip_moves() {
    use diesel::{QueryDsl as _, RunQueryDsl as _};

    use crate::models::{
        network::db::{create, save},
        schema::network,
    };
    let (_storage, db, _directory) = storage().await;
    let rows = db
        .write(|connection| {
            let network = network();
            // The start's row: nothing read yet.
            create(connection, &network)?;
            let mut rows = Vec::new();
            for (tip, indexed, now) in [(10, Some(4), 1), (10, None, 2), (12, None, 3)] {
                save(
                    connection,
                    &network,
                    tip,
                    tip.saturating_sub(6),
                    indexed,
                    now,
                )?;
                rows.push(
                    network::table
                        .select((network::confirmed, network::indexed, network::checked_at))
                        .first::<(Option<i64>, Option<i64>, Option<i64>)>(connection)?,
                );
            }
            Ok(rows)
        })
        .await
        .unwrap();
    assert_eq!(
        rows,
        [
            (Some(4), Some(4), Some(1)),
            (Some(4), Some(4), Some(1)),
            (Some(6), Some(4), Some(3))
        ]
    );
}
