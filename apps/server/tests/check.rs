//! The broadcast job's checks (designs 084, 085) over the scripted network
//! on SQLite, each by the payment's transaction index alone: a sent payment
//! holds its funds and coins whatever its age and is sent again at each
//! check whatever the answer, until it is found in a final block; one not
//! found past its `expires_height` is replaced while bumps remain, else
//! forgotten (`dead`) and its job fails; one whose index is not read
//! through the final block is not sent again; one whose index was given
//! up, or a jump forgot, fails `forgotten`. One missing for `bump_after`
//! blocks its index read is replaced: the row swapped, the funds held once,
//! both versions watched; refused, undone; at the fee cap or `max_bumps`,
//! sent again, stuck; whichever version lands ends it. The job's end
//! deletes every index.

use std::sync::Arc;

use base::{
    Address, BlockStore, Decimal, FeeLevel, IndexState, InputStore as _, Tracking, Transaction,
    TransactionErrorKind, TransactionIndexStore, TransactionState,
};
use tasks::{Ack, Admission, Claim, Context, Id, OnLock, Store, Worker};

use super::{
    Fake, Script, container, network,
    storage::{
        address, block, coin, confirm, forget, forgotten, look_for, looked_for, mined, payment,
        raw, record, run, send, storage, watch,
    },
};
use crate::{
    db::Db,
    models::transaction_forgotten::Why,
    usecase::{
        sdk::Stores,
        tasks::Queue,
        workers::broadcast::{Broadcast, Progress, SendIntent},
    },
};

/// alice, indexed through block 1, holds coin `f:0` (10) of block 0 and has
/// sent `s` of it: 4 to bob, fee 1, its index (from 0, expiring past
/// `expires`) read through 1, the confirmed block. Sent at Unix ms 0, older
/// than any expiry there was; its broadcast job is due for a check.
struct Fixture {
    storage: Stores,
    db: Db,
    queue: Queue,
    script: Arc<Script>,
    worker: Broadcast,
    job: Id,
    directory: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        Self::expiring(None).await
    }

    async fn expiring(expires: Option<u64>) -> Self {
        Self::with(expires, Script::default()).await
    }

    /// Bumped once its index read `bump_after` blocks, at most `max_bumps`
    /// times; signing a replacement fails as `bump` says.
    async fn bumping(bump_after: u64, max_bumps: u32, bump: Option<TransactionErrorKind>) -> Self {
        let tracking = Tracking {
            bump_after,
            max_bumps,
            ..Tracking::default()
        };
        let script = Script {
            tracking,
            bump,
            ..Script::default()
        };
        Self::with(None, script).await
    }

    /// Expired past height 0 unseen (Solana), so dead once its index read
    /// block 1; bumped then, at most `max_bumps` times.
    async fn dying(max_bumps: u32) -> Self {
        let tracking = Tracking {
            max_bumps,
            ..Tracking::default()
        };
        let script = Script {
            tracking,
            ..Script::default()
        };
        Self::with(Some(0), script).await
    }

    async fn with(expires: Option<u64>, script: Script) -> Self {
        let (storage, db, directory) = storage().await;
        let network = network();
        watch(&storage, &alice(), 0).await;
        look_for(&storage, "s", 0, expires).await;
        let funded = vec![
            block(
                &network,
                0,
                0,
                vec![payment(&network, "f", "alice", 10, None)],
            ),
            block(&network, 1, 0, Vec::new()),
        ];
        BlockStore::insert(&storage, &run(&network, funded))
            .await
            .unwrap();
        confirm(&db, 1).await;
        record(&db, &sent("s", "bob", 4, "f")).await;
        script.signed.lock().unwrap().push(sent("s", "bob", 4, "f"));
        let script = Arc::new(script);
        // Keyed: a bump signs as alice.
        let keyed = Fake {
            address: alice(),
            script: script.clone(),
        };
        base::WalletStore::insert(&storage, &keyed).await.unwrap();
        let queue = crate::usecase::tasks::queue(db.clone());
        let intent = SendIntent {
            chain: network.chain.to_string(),
            network: network.name.clone(),
            address: "alice".to_owned(),
            asset: "native".to_owned(),
            to: "bob".to_owned(),
            amount: "4".to_owned(),
            fee: FeeLevel::Normal,
        };
        let Admission::Queued(job) = Broadcast::enqueue(&queue, &intent).await.unwrap() else {
            panic!("a send is always queued");
        };
        let now = tasks::now();
        let claim = Claim {
            kind: Broadcast::KIND,
            limit: 1,
            on_lock: OnLock::Wait,
            now,
            until: now + 60_000,
        };
        let claimed = queue.claim(claim).await.unwrap();
        assert_eq!(claimed.len(), 1, "the job runs");
        let fixture = Self {
            worker: Broadcast::new(container(&db, script.clone()), db.clone()),
            storage,
            db,
            queue,
            script,
            job,
            directory,
        };
        // It sent `s` on its first run.
        fixture.save(&progress("s", &[], 0)).await;
        fixture
    }

    /// Stores `progress` as the job's state.
    async fn save(&self, progress: &Progress) {
        let mut job = self.queue.get(self.job).await.unwrap().unwrap();
        job.state = Some(serde_json::to_value(progress).unwrap());
        let attempt = job.attempts;
        self.queue.update(job, attempt).await.unwrap();
    }

    /// The job's state: the current txid, those it replaced, its bumps.
    async fn progress(&self) -> (String, Vec<String>, u32) {
        let job = self.queue.get(self.job).await.unwrap().unwrap();
        let progress: Progress = serde_json::from_value(job.state.unwrap()).unwrap();
        (progress.txid, progress.replaced, progress.bumps)
    }

    /// One check.
    async fn check(&self) -> Ack {
        let mut job = self.queue.get(self.job).await.unwrap().unwrap();
        let store: Arc<dyn Store> = Arc::new(self.queue.clone());
        self.worker
            .run(Context::new(&mut job, store).unwrap())
            .await
    }

    /// Whether `txid` is still kept, pending or mined.
    async fn kept(&self, txid: &str) -> bool {
        let id = format!("bitcoin.regtest.{txid}");
        self.db
            .read(move |connection| {
                Ok(crate::models::transaction::db::get(connection, &id)?.is_some())
            })
            .await
            .unwrap()
    }

    /// alice's unspent coins, and what she can spend.
    async fn holds(&self) -> (usize, Decimal) {
        let unspent = self.storage.find_unspent(&[alice()]).await.unwrap().len();
        let spendable = self
            .db
            .read(|connection| {
                let id = "bitcoin.regtest.alice.native";
                let indexes =
                    crate::models::wallet_index::db::find_by_ids(connection, &network(), &[id], 1)?;
                indexes[id].spendable()
            })
            .await
            .unwrap();
        (unspent, spendable)
    }

    /// Keeps block 2 with `transactions` in it.
    async fn mine(&self, transactions: Vec<Transaction>) {
        let network = network();
        BlockStore::insert(
            &self.storage,
            &run(&network, vec![block(&network, 2, 0, transactions)]),
        )
        .await
        .unwrap();
    }

    /// What was sent again.
    fn sent(&self) -> Vec<Vec<u8>> {
        self.script.sent.lock().unwrap().clone()
    }

    /// The txids looked for: the job's end deletes its index.
    async fn looked_for(&self) -> Vec<String> {
        let indexes = looked_for(&self.storage).await;
        indexes.into_iter().map(|index| index.0).collect()
    }
}

fn alice() -> Address {
    address(&network(), "alice")
}

/// A sent payment's state: current `txid`, its builder's state, having
/// replaced `replaced`.
fn progress(txid: &str, replaced: &[&str], bumps: u32) -> Progress {
    Progress {
        txid: txid.to_owned(),
        sent: true,
        replaced: replaced.iter().map(|&txid| txid.to_owned()).collect(),
        bumps,
        builder: Some(txid.into()),
    }
}

/// Pending `txid` of alice's coin `spent`: `amount` to `to`, fee 1.
fn sent(txid: &str, to: &str, amount: u64, spent: &str) -> Transaction {
    let network = network();
    let legs = send(&network, "alice", to, amount, 1, coin(&network, spent, 0));
    let mut transaction = mined(&network, txid, 0, legs);
    transaction.state = TransactionState::Pending;
    transaction.order = None;
    transaction
}

#[tokio::test]
async fn a_sent_payment_is_sent_again_at_each_check_until_it_is_found_final() {
    let fixture = Fixture::new().await;
    fixture
        .script
        .answers
        .lock()
        .unwrap()
        .push_back(Err(TransactionErrorKind::Rejected));
    for _ in 0..2 {
        assert!(matches!(fixture.check().await, Ack::Again(_, None)));
    }
    assert!(
        fixture.kept("s").await,
        "a refused send again forgets nothing"
    );
    assert_eq!(fixture.holds().await, (0, Decimal::from(5)));
    let s = b"s".to_vec();
    assert_eq!(fixture.sent(), [s.clone(), s]);
    let network = network();
    let legs = send(&network, "alice", "bob", 4, 1, coin(&network, "f", 0));
    fixture.mine(vec![mined(&network, "s", 0, legs)]).await;
    assert!(
        matches!(fixture.check().await, Ack::Again(_, None)),
        "found above the confirmed block: not final yet"
    );
    assert_eq!(fixture.sent().len(), 2, "a found one is not sent again");
    confirm(&fixture.db, 2).await;
    assert_eq!(fixture.check().await, Ack::Done);
    assert!(forgotten(&fixture.directory).is_empty());
    assert!(fixture.looked_for().await.is_empty());
}

#[tokio::test]
async fn a_payment_not_found_past_its_expires_height_is_dead() {
    let fixture = Fixture::expiring(Some(0)).await;
    assert_eq!(fixture.check().await, Ack::Fail("dead".into()));
    assert!(!fixture.kept("s").await);
    assert_eq!(fixture.holds().await, (1, Decimal::from(10)));
    assert!(fixture.sent().is_empty(), "not sent again");
    assert!(fixture.looked_for().await.is_empty());
    assert_eq!(
        forgotten(&fixture.directory),
        [("s".to_owned(), "dead".to_owned())]
    );
    // Who was to be paid what, and its bytes, as recorded.
    #[derive(diesel::QueryableByName)]
    struct Kept {
        #[diesel(sql_type = diesel::sql_types::Text)]
        legs: String,
        #[diesel(sql_type = diesel::sql_types::Binary)]
        bytes: Vec<u8>,
    }
    let kept: Vec<Kept> = diesel::RunQueryDsl::load(
        diesel::sql_query("SELECT legs, bytes FROM transaction_forgotten"),
        &mut raw(&fixture.directory),
    )
    .unwrap();
    let legs: serde_json::Value = serde_json::from_str(&kept[0].legs).unwrap();
    let kinds: Vec<_> = legs
        .as_array()
        .unwrap()
        .iter()
        .map(|leg| (leg["kind"].clone(), leg["amount"].clone()))
        .collect();
    assert_eq!(
        kinds,
        [
            (serde_json::json!(1), serde_json::json!("4")),
            (serde_json::json!(4), serde_json::json!("1"))
        ],
        "alice's spend and fee; bob is not indexed"
    );
    assert_eq!(kept[0].bytes, b"s");
}

#[tokio::test]
async fn a_payment_found_before_its_expires_height_is_done() {
    let fixture = Fixture::expiring(Some(1)).await;
    let network = network();
    let legs = send(&network, "alice", "bob", 4, 1, coin(&network, "f", 0));
    fixture.mine(vec![mined(&network, "s", 0, legs)]).await;
    confirm(&fixture.db, 2).await;
    assert_eq!(fixture.check().await, Ack::Done);
    assert!(forgotten(&fixture.directory).is_empty());
}

#[tokio::test]
async fn a_payment_found_reverted_in_a_final_block_fails_reverted() {
    let fixture = Fixture::new().await;
    let network = network();
    let mut legs = send(&network, "alice", "bob", 4, 1, coin(&network, "f", 0));
    let mut reverted = mined(&network, "s", 0, vec![legs.remove(2)]);
    reverted.state = TransactionState::Reverted(None);
    fixture.mine(vec![reverted]).await;
    confirm(&fixture.db, 2).await;
    assert_eq!(fixture.check().await, Ack::Fail("reverted".into()));
    assert!(fixture.kept("s").await, "mined: kept, not forgotten");
    assert!(forgotten(&fixture.directory).is_empty());
    assert!(fixture.looked_for().await.is_empty());
}

#[tokio::test]
async fn a_payment_whose_index_is_not_read_through_the_final_block_waits() {
    let fixture = Fixture::new().await;
    confirm(&fixture.db, 5).await;
    assert!(matches!(fixture.check().await, Ack::Again(..)));
    assert!(fixture.kept("s").await);
    assert!(fixture.sent().is_empty(), "not sent again");
}

#[tokio::test]
async fn a_payment_whose_index_was_given_up_fails_forgotten() {
    let fixture = Fixture::new().await;
    let index = TransactionIndexStore::find_all(&fixture.storage, &network())
        .await
        .unwrap()
        .remove(0);
    fixture.storage.give_up(&index).await.unwrap();
    assert_eq!(fixture.check().await, Ack::Fail("forgotten".into()));
    assert!(!fixture.kept("s").await);
    assert!(fixture.sent().is_empty());
    assert!(fixture.looked_for().await.is_empty());
    assert_eq!(
        forgotten(&fixture.directory),
        [("s".to_owned(), "jump".to_owned())]
    );
}

#[tokio::test]
async fn a_payment_a_jump_forgot_fails_forgotten() {
    let fixture = Fixture::new().await;
    assert_eq!(forget(&fixture.db, "s", Why::Jump).await, 1);
    assert_eq!(fixture.check().await, Ack::Fail("forgotten".into()));
    assert!(fixture.sent().is_empty());
    assert!(fixture.looked_for().await.is_empty());
}

#[tokio::test]
async fn a_senders_pending_sends_are_those_it_pays_from() {
    let fixture = Fixture::new().await;
    watch(&fixture.storage, &address(&network(), "bob"), 0).await;
    record(&fixture.db, &sent("t", "bob", 1, "g")).await;
    let txids = |sent: Vec<Transaction>| -> Vec<String> {
        let mut txids: Vec<_> = sent
            .into_iter()
            .map(|transaction| transaction.id.id)
            .collect();
        txids.sort();
        txids
    };
    let of_alice = fixture.storage.find_sent(&alice()).await.unwrap();
    assert_eq!(txids(of_alice), ["s", "t"]);
    let of_bob = fixture.storage.find_sent(&address(&network(), "bob")).await;
    assert!(of_bob.unwrap().is_empty(), "bob only receives");
}

/// The txids looked for, each with its state.
async fn watched(fixture: &Fixture) -> Vec<(String, IndexState)> {
    let indexes = looked_for(&fixture.storage).await;
    let watched = indexes.into_iter().map(|(txid, _, _, state)| (txid, state));
    watched.collect()
}

#[tokio::test]
async fn a_payment_missing_for_bump_after_blocks_read_is_replaced() {
    let fixture = Fixture::bumping(1, 2, None).await;
    assert!(matches!(fixture.check().await, Ack::Again(_, None)));
    assert_eq!(fixture.sent(), [b"s+".to_vec()], "the replacement, sent");
    assert!(!fixture.kept("s").await);
    assert!(fixture.kept("s+").await);
    assert_eq!(
        forgotten(&fixture.directory),
        [("s".to_owned(), "replaced".to_owned())]
    );
    assert_eq!(
        fixture.holds().await,
        (0, Decimal::from(4)),
        "held once: 4 and the new fee, 2"
    );
    let indexing = IndexState::Indexing;
    assert_eq!(
        watched(&fixture).await,
        [("s".to_owned(), indexing), ("s+".to_owned(), indexing)],
        "the old version watched on"
    );
    assert_eq!(
        fixture.progress().await,
        ("s+".to_owned(), vec!["s".to_owned()], 1)
    );
}

#[tokio::test]
async fn a_payment_is_not_bumped_before_its_index_read_bump_after_blocks() {
    let fixture = Fixture::bumping(2, 2, None).await;
    assert!(matches!(fixture.check().await, Ack::Again(_, None)));
    assert_eq!(fixture.sent(), [b"s".to_vec()], "sent again, not bumped");
    assert!(forgotten(&fixture.directory).is_empty());
    // One block more read past its broadcast: due.
    fixture.mine(Vec::new()).await;
    confirm(&fixture.db, 2).await;
    assert!(matches!(fixture.check().await, Ack::Again(_, None)));
    assert_eq!(fixture.sent(), [b"s".to_vec(), b"s+".to_vec()]);
}

#[tokio::test]
async fn a_payment_at_max_bumps_or_the_fee_cap_is_sent_again_stuck() {
    let capped = Fixture::bumping(1, 2, Some(TransactionErrorKind::FeeCap)).await;
    let at_max = Fixture::bumping(1, 2, None).await;
    at_max.save(&progress("s", &[], 2)).await;
    for fixture in [capped, at_max] {
        let Ack::Again(_, Some(why)) = fixture.check().await else {
            panic!("checked again, with why");
        };
        assert!(why.starts_with("stuck: "), "{why}");
        assert_eq!(fixture.sent(), [b"s".to_vec()], "the same bytes");
        assert!(fixture.kept("s").await);
        assert!(forgotten(&fixture.directory).is_empty());
    }
}

#[tokio::test]
async fn a_payment_without_a_builder_state_is_sent_again_never_bumped() {
    let fixture = Fixture::bumping(1, 2, None).await;
    let progress = Progress {
        builder: None,
        ..progress("s", &[], 0)
    };
    fixture.save(&progress).await;
    assert!(matches!(fixture.check().await, Ack::Again(_, None)));
    assert_eq!(fixture.sent(), [b"s".to_vec()], "the same bytes");
    assert!(forgotten(&fixture.directory).is_empty());
}

#[tokio::test]
async fn a_replaced_version_found_final_is_done_and_the_current_one_forgotten() {
    let fixture = Fixture::bumping(1, 2, None).await;
    assert!(matches!(fixture.check().await, Ack::Again(_, None)));
    let network = network();
    let legs = send(&network, "alice", "bob", 4, 1, coin(&network, "f", 0));
    fixture.mine(vec![mined(&network, "s", 0, legs)]).await;
    confirm(&fixture.db, 2).await;
    assert_eq!(fixture.check().await, Ack::Done);
    assert!(fixture.kept("s").await, "mined");
    assert!(!fixture.kept("s+").await);
    assert_eq!(
        forgotten(&fixture.directory),
        [
            ("s".to_owned(), "replaced".to_owned()),
            ("s+".to_owned(), "replaced".to_owned())
        ]
    );
    assert_eq!(fixture.holds().await, (0, Decimal::from(5)));
    assert!(fixture.looked_for().await.is_empty(), "every index deleted");
}

#[tokio::test]
async fn a_replacement_every_node_refuses_is_undone() {
    let fixture = Fixture::bumping(1, 2, None).await;
    fixture
        .script
        .answers
        .lock()
        .unwrap()
        .push_back(Err(TransactionErrorKind::Rejected));
    let Ack::Again(_, Some(why)) = fixture.check().await else {
        panic!("checked again, with why");
    };
    assert!(why.starts_with("refused: "), "{why}");
    assert!(fixture.kept("s").await, "the old version current again");
    assert!(!fixture.kept("s+").await);
    assert_eq!(
        forgotten(&fixture.directory),
        [("s+".to_owned(), "refused".to_owned())]
    );
    assert_eq!(fixture.holds().await, (0, Decimal::from(5)));
    assert_eq!(fixture.looked_for().await, ["s"]);
    assert_eq!(fixture.progress().await, ("s".to_owned(), Vec::new(), 1));
}

#[tokio::test]
async fn a_dead_payment_is_replaced_and_its_new_version_found_final_is_done() {
    let fixture = Fixture::dying(2).await;
    assert!(matches!(fixture.check().await, Ack::Again(_, None)));
    assert_eq!(fixture.sent(), [b"s+".to_vec()], "the new version, sent");
    assert!(!fixture.kept("s").await);
    assert!(fixture.kept("s+").await);
    assert_eq!(
        forgotten(&fixture.directory),
        [("s".to_owned(), "replaced".to_owned())]
    );
    assert_eq!(
        fixture.holds().await,
        (0, Decimal::from(4)),
        "held once: 4 and the new fee, 2"
    );
    let indexing = IndexState::Indexing;
    assert_eq!(
        watched(&fixture).await,
        [("s".to_owned(), indexing), ("s+".to_owned(), indexing)],
        "every version watched"
    );
    assert_eq!(
        fixture.progress().await,
        ("s+".to_owned(), vec!["s".to_owned()], 1)
    );
    // Its index reads from block 2, where it lands.
    let next = TransactionIndexStore::find_all(&fixture.storage, &network())
        .await
        .unwrap()
        .into_iter()
        .find(|index| index.transaction.id == "s+")
        .unwrap();
    fixture.storage.move_start(&next, 2).await.unwrap();
    let network = network();
    let legs = send(&network, "alice", "bob", 4, 2, coin(&network, "f", 0));
    fixture.mine(vec![mined(&network, "s+", 0, legs)]).await;
    confirm(&fixture.db, 2).await;
    assert_eq!(fixture.check().await, Ack::Done);
    assert!(fixture.kept("s+").await, "mined");
    assert_eq!(
        forgotten(&fixture.directory),
        [("s".to_owned(), "replaced".to_owned())]
    );
    assert!(fixture.looked_for().await.is_empty(), "every index deleted");
}

#[tokio::test]
async fn a_dead_payment_at_max_bumps_fails_dead() {
    let fixture = Fixture::dying(2).await;
    fixture.save(&progress("s", &[], 2)).await;
    assert_eq!(fixture.check().await, Ack::Fail("dead".into()));
    assert!(fixture.sent().is_empty(), "not sent again");
    assert_eq!(
        forgotten(&fixture.directory),
        [("s".to_owned(), "dead".to_owned())]
    );
    assert_eq!(fixture.holds().await, (1, Decimal::from(10)));
    assert!(fixture.looked_for().await.is_empty());
}

#[test]
fn a_job_queued_before_fee_levels_and_bumps_still_decodes() {
    let intent = serde_json::json!({
        "chain": "bitcoin", "network": "regtest", "address": "alice",
        "asset": "native", "to": "bob", "amount": "4"
    });
    let intent: SendIntent = serde_json::from_value(intent).unwrap();
    assert_eq!(intent.fee, FeeLevel::Normal);
    let state = serde_json::json!({"txid": "s", "sent": true});
    let state: Progress = serde_json::from_value(state).unwrap();
    assert_eq!((state.replaced.len(), state.bumps), (0, 0));
    assert!(state.builder.is_none());
}
