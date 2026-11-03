//! The server's `models` and stores (`usecase`) on SQLite temp files: the `base`
//! store contracts, and the balance and history reads. Offline and
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

use crate::{db::Db, usecase::sdk::Stores};

async fn storage() -> (Stores, Db, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let db = crate::db::Db::open(&directory.path().join("test.db")).unwrap();
    db.migrate(&crate::schema::ALL).await.unwrap();
    (Stores::new(db.clone()), db, directory)
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

/// Records `transaction` as pending, as the queued broadcaster does.
async fn record(db: &Db, transaction: &Transaction) {
    let transaction = transaction.clone();
    db.write(move |connection| {
        crate::models::transaction::db::insert_pending(connection, &transaction, 0)
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
        crate::usecase::history::balance(&db, &alice, &native(&network))
            .await
            .unwrap(),
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
        crate::usecase::history::balance(&db, &alice, &native(&network))
            .await
            .unwrap(),
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
        crate::usecase::history::balance(&db, &alice, &native(&network))
            .await
            .unwrap(),
        Decimal::zero()
    );
    assert!(
        storage
            .find_unspent(std::slice::from_ref(&alice))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        crate::usecase::history::history(&db, &alice, &native(&network), None, 10)
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
async fn balance_and_history_read_live_blocks_only() {
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
        crate::usecase::history::balance(&db, &alice, &asset)
            .await
            .unwrap(),
        Decimal::from(3)
    );

    let first = crate::usecase::history::history(&db, &alice, &asset, None, 1)
        .await
        .unwrap();
    let ids: Vec<_> = first.iter().map(|(_, t)| t.id.id.clone()).collect();
    assert_eq!(ids, ["a"]);
    let after = Some((0, 0, "a".to_owned()));
    let second = crate::usecase::history::history(&db, &alice, &asset, after, 10)
        .await
        .unwrap();
    assert_eq!(second.len(), 1);
    let (header, transaction) = &second[0];
    assert_eq!((header.position, transaction.id.id.as_str()), (1, "b"));
    // Only alice's own legs: the spend, not bob's receipt.
    assert!(matches!(transaction.legs[..], [Leg::Spent { .. }]));

    BlockStore::delete(&storage, &network, 1).await.unwrap();
    assert_eq!(
        crate::usecase::history::balance(&db, &alice, &asset)
            .await
            .unwrap(),
        Decimal::from(6)
    );
    assert_eq!(
        crate::usecase::history::history(&db, &alice, &asset, None, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    let other = address(&other(), "alice");
    assert_eq!(
        crate::usecase::history::balance(&db, &other, &native(&other.network))
            .await
            .unwrap(),
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
    connection
}

#[tokio::test]
async fn the_database_refuses_a_parent_that_is_not_stored() {
    use diesel::connection::SimpleConnection as _;
    let (storage, _db, directory) = storage().await;
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
