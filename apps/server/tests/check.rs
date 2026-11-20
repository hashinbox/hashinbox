//! The broadcast job's checks (designs 084, 085, 087) over the scripted
//! network on SQLite, each by the payment's transaction index alone: a sent
//! payment holds its funds and coins whatever its age and is sent again at
//! each check whatever the answer, until it is found in a final block; one
//! mined above it waits, and a reorg of its block puts it back, its spend
//! held, sent again; one that expired (its index read through the final
//! block, not found, that block's height above its `expires_height`) is
//! dropped, its spend released, and signed anew while bumps remain, else
//! its job fails `expired`; one not past it yet is sent again; one whose index is not
//! read through the final block is not sent again; one whose index was
//! given up, or a jump forgot, fails `forgotten`. One missing for
//! `bump_after` blocks its index read is replaced: the row swapped, the
//! funds held once, both versions watched; refused, undone; out of bumps or
//! at the fee cap, cancelled; no cancel either, sent again, stuck;
//! whichever version lands ends it. The job's end deletes every pending
//! version and every index; its state keeps the outcome.

use std::sync::Arc;

use base::{
    Address, BlockStore, Decimal, FeeLevel, Holding, IndexState, Indexes as _, InputStore as _,
    Tracking, Transaction, TransactionErrorKind, TransactionIndexStore, TransactionIndexer,
    TransactionState,
    testing::{
        Confirms, Keys, Served,
        fixtures::{block, coin, payment, quiet, run},
    },
};
use tasks::{
    Ack, Admission, Id, Store, Worker,
    testing::{claim, run_once},
};

use super::{
    storage::{
        address, confirm, forget, look_for, looked_for, mined, raw, record, rows, send, watch,
    },
    support::{
        db::seeded,
        scripted::{Script, container, network},
    },
};
use crate::{
    db::Db,
    models::transaction_index::Why,
    usecase::{
        sdk::Stores,
        tasks::Queue,
        workers::broadcast::{Broadcast, Outcome, Progress, SendIntent},
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
            keys: Keys::new(network()).with_failures(bump, None),
            ..Script::default()
        };
        Self::with(None, script).await
    }

    /// Expiring past height 0 (Solana), so expired: its index read through
    /// block 1, final at height 1, unseen; signed anew then, at most
    /// `max_bumps` times.
    async fn expired(max_bumps: u32) -> Self {
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
        let (storage, db, directory) = seeded().await;
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
        script.keys.signed(sent("s", "bob", 4, "f"));
        let script = Arc::new(script);
        // Keyed: a bump signs as alice.
        let keyed = script.keys.keyed("alice");
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
        let claimed = claim(&queue, Broadcast::KIND, 1, tasks::now()).await;
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

    /// The job's whole state.
    async fn state(&self) -> Progress {
        let job = self.queue.get(self.job).await.unwrap().unwrap();
        serde_json::from_value(job.state.unwrap()).unwrap()
    }

    /// The job's outcome and reason, and every txid it sent.
    async fn ended(&self) -> (Option<Outcome>, Option<String>, Vec<String>) {
        let state = self.state().await;
        let txids = [state.replaced, vec![state.txid]].concat();
        (state.outcome, state.reason, txids)
    }

    /// What a payment leaves pending: transaction rows not mined, and
    /// transactions' legs not mined.
    fn pending(&self) -> (i64, i64) {
        #[derive(diesel::QueryableByName)]
        struct Count {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            rows: i64,
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            legs: i64,
        }
        let count: Count = diesel::RunQueryDsl::get_result(
            diesel::sql_query(
                r#"SELECT
                    (SELECT COUNT(*) FROM "transaction" WHERE block IS NULL) AS rows,
                    (SELECT COUNT(*) FROM leg
                     WHERE "transaction" IS NOT NULL AND position IS NULL) AS legs"#,
            ),
            &mut raw(&self.directory),
        )
        .unwrap();
        (count.rows, count.legs)
    }

    /// How many legs the payment's versions (`s`, `s+`, …) hold, pending
    /// or mined.
    fn held(&self) -> i64 {
        #[derive(diesel::QueryableByName)]
        struct Count {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            held: i64,
        }
        let count: Count = diesel::RunQueryDsl::get_result(
            diesel::sql_query(
                r#"SELECT COUNT(*) AS held FROM leg
                WHERE "transaction" LIKE 'bitcoin.regtest.s%'"#,
            ),
            &mut raw(&self.directory),
        )
        .unwrap();
        count.held
    }

    /// Moves the start of `txid`'s index to `at`.
    async fn start(&self, txid: &str, at: u64) {
        let index = TransactionIndexStore::find_all(&self.storage, &network())
            .await
            .unwrap()
            .into_iter()
            .find(|index| index.transaction.id == txid)
            .unwrap();
        self.storage.move_start(&index, at).await.unwrap();
    }

    /// One check.
    async fn check(&self) -> Ack {
        run_once(&self.worker, &self.queue, self.job).await
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

    /// Closes `s`'s index through the chain's `Confirmer`, final through 5:
    /// found in block 3, `reverted` there or not; then 5 is confirmed.
    async fn settle(&self, reverted: bool) {
        let network = network();
        let confirms = Arc::new(Confirms::new(network.clone()));
        confirms.set("s", Some((5, Some(3), reverted)));
        let closer = TransactionIndexer::new(
            Arc::new(self.storage.clone()),
            Arc::new(Served::new(quiet(&network, 5), 0)),
            Some(confirms),
        );
        closer.close(5, 5).await.unwrap();
        confirm(&self.db, 5).await;
    }

    /// What was sent again.
    fn sent(&self) -> Vec<Vec<u8>> {
        self.script.broadcasts.sent()
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
        cancels: Vec::new(),
        outcome: None,
        reason: None,
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
        .broadcasts
        .answer(Err(TransactionErrorKind::Rejected));
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
    assert!(fixture.looked_for().await.is_empty());
    assert_eq!(fixture.ended().await.0, Some(Outcome::Final));
}

#[tokio::test]
async fn an_expired_payment_with_bumps_off_ends_expired_and_leaves_nothing() {
    let fixture = Fixture::expiring(Some(0)).await;
    assert_eq!(fixture.check().await, Ack::Fail("expired".into()));
    assert!(!fixture.kept("s").await);
    assert_eq!(
        fixture.holds().await,
        (1, Decimal::from(10)),
        "its spend released: nothing mined"
    );
    assert!(fixture.sent().is_empty(), "not sent again");
    assert!(fixture.looked_for().await.is_empty());
    assert_eq!(fixture.pending(), (0, 0), "no pending row, no pending leg");
    assert_eq!(
        fixture.ended().await,
        (
            Some(Outcome::Expired),
            Some("expired".into()),
            vec!["s".into()]
        ),
        "the job's state is the record"
    );
}

#[tokio::test]
async fn a_payment_whose_final_height_is_not_past_its_expiry_is_sent_again() {
    let fixture = Fixture::expiring(Some(1)).await;
    assert!(matches!(fixture.check().await, Ack::Again(_, None)));
    assert_eq!(fixture.sent(), [b"s".to_vec()], "the same bytes again");
    assert!(fixture.kept("s").await);
    assert_eq!(fixture.holds().await, (0, Decimal::from(5)), "still held");
    assert_eq!(
        watched(&fixture).await,
        [("s".to_owned(), IndexState::Indexing)]
    );
}

#[tokio::test]
async fn a_payment_found_before_its_expires_height_is_done() {
    let fixture = Fixture::expiring(Some(1)).await;
    let network = network();
    let legs = send(&network, "alice", "bob", 4, 1, coin(&network, "f", 0));
    fixture.mine(vec![mined(&network, "s", 0, legs)]).await;
    confirm(&fixture.db, 2).await;
    assert_eq!(fixture.check().await, Ack::Done);
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
    assert!(
        fixture.kept("s").await,
        "mined: kept, its fee chain history"
    );
    assert!(fixture.looked_for().await.is_empty());
    assert_eq!(fixture.ended().await.0, Some(Outcome::Reverted));
}

#[tokio::test]
async fn a_payment_settled_through_the_confirmer_ends_by_its_outcome() {
    for (reverted, ack) in [(true, Ack::Fail("reverted".into())), (false, Ack::Done)] {
        let fixture = Fixture::new().await;
        fixture.settle(reverted).await;
        assert_eq!(fixture.check().await, ack, "reverted: {reverted}");
        assert!(fixture.sent().is_empty(), "found: not sent again");
        assert!(fixture.looked_for().await.is_empty());
    }
}

/// s is mined at 3, a block alice's index never reads: she settles at 5 at
/// the chain's 5, s already out of it, while s's held pending legs still
/// subtract, so spendable counts s twice (0, not 5) until the job ends. s's
/// index is closed through the `Confirmer`; the job ends done and deletes
/// s's pending row and legs: every balance is the chain's 5.
#[tokio::test]
async fn a_settle_past_a_payment_counts_it_twice_until_its_job_ends() {
    let fixture = Fixture::new().await;
    let balances = async || {
        let index = fixture
            .db
            .read(|connection| {
                let id = "bitcoin.regtest.alice.native";
                let mut indexes =
                    crate::models::wallet_index::db::find_by_ids(connection, &network(), &[id], 5)?;
                Ok(indexes.remove(id).unwrap())
            })
            .await
            .unwrap();
        let parse = |total: Option<String>| total.unwrap().parse::<Decimal>().unwrap();
        [
            parse(index.balance_confirmed),
            parse(index.balance_confirmed_spendable),
            parse(index.balance_pending),
        ]
    };
    confirm(&fixture.db, 5).await;
    let alice = base::WalletIndexStore::find_all(&fixture.storage, &network())
        .await
        .unwrap()
        .remove(0);
    let chain = Holding {
        position: 5,
        amount: Decimal::from(5),
    };
    assert!(
        base::WalletIndexStore::settle(&fixture.storage, &alice, &chain)
            .await
            .unwrap()
    );
    let [confirmed, spendable, pending] = balances().await;
    assert_eq!(confirmed, Decimal::from(5), "the chain's balance at 5");
    assert_eq!(
        confirmed - spendable,
        Decimal::from(5),
        "s's 4 and fee 1 counted twice: in the settle and held"
    );
    assert_eq!(pending, Decimal::from(0), "the legs subtract too");
    assert_eq!(fixture.pending(), (1, 2), "s pending, its two legs");

    fixture.settle(false).await;
    assert_eq!(fixture.check().await, Ack::Done);
    assert_eq!(
        balances().await,
        [5, 5, 5].map(Decimal::from),
        "counted once"
    );
    assert_eq!(fixture.pending(), (0, 0), "no pending row, no pending leg");
    assert_eq!(fixture.held(), 0);
    assert!(
        fixture.looked_for().await.is_empty(),
        "no transaction index"
    );
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
    assert_eq!(fixture.ended().await.0, Some(Outcome::Forgotten));
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
    assert!(fixture.kept("s").await);
    // One block more read past its broadcast: due.
    fixture.mine(Vec::new()).await;
    confirm(&fixture.db, 2).await;
    assert!(matches!(fixture.check().await, Ack::Again(_, None)));
    assert_eq!(fixture.sent(), [b"s".to_vec(), b"s+".to_vec()]);
}

#[tokio::test]
async fn a_payment_out_of_bumps_or_at_the_fee_cap_is_cancelled() {
    let capped = Fixture::bumping(1, 2, Some(TransactionErrorKind::FeeCap)).await;
    let at_max = Fixture::bumping(1, 2, None).await;
    at_max.save(&progress("s", &[], 2)).await;
    for fixture in [capped, at_max] {
        assert!(matches!(fixture.check().await, Ack::Again(_, None)));
        assert_eq!(fixture.sent(), [b"sx".to_vec()], "the cancel, sent");
        assert!(!fixture.kept("s").await);
        assert!(fixture.kept("sx").await);
        assert_eq!(
            fixture.holds().await.1,
            Decimal::from(8),
            "only the cancel's fee, 2, held"
        );
        let state = fixture.state().await;
        assert_eq!(
            (state.txid, state.cancels),
            ("sx".into(), vec!["sx".into()])
        );
    }
}

#[tokio::test]
async fn a_payment_no_cancel_can_replace_is_sent_again_stuck() {
    use TransactionErrorKind::FeeCap;
    let tracking = Tracking {
        bump_after: 1,
        max_bumps: 2,
        ..Tracking::default()
    };
    let script = Script {
        tracking,
        keys: Keys::new(network()).with_failures(Some(FeeCap), Some(FeeCap)),
        ..Script::default()
    };
    let fixture = Fixture::with(None, script).await;
    let Ack::Again(_, Some(why)) = fixture.check().await else {
        panic!("checked again, with why");
    };
    assert!(why.starts_with("stuck: "), "{why}");
    assert_eq!(fixture.sent(), [b"s".to_vec()], "the same bytes");
    assert!(fixture.kept("s").await);
}

/// A payment out of bumps, cancelled (`sx`, fee 2), the cancel's index
/// reading from block 2.
async fn cancelled() -> Fixture {
    let fixture = Fixture::bumping(1, 1, None).await;
    fixture.save(&progress("s", &[], 1)).await;
    assert!(matches!(fixture.check().await, Ack::Again(_, None)));
    assert_eq!(fixture.sent(), [b"sx".to_vec()]);
    fixture.start("sx", 2).await;
    fixture
}

#[tokio::test]
async fn a_cancel_landing_final_fails_cancelled_and_releases_the_payment() {
    let fixture = cancelled().await;
    let network = network();
    let fee = send(&network, "alice", "alice", 0, 2, coin(&network, "f", 0)).remove(2);
    fixture
        .mine(vec![mined(&network, "sx", 0, vec![fee])])
        .await;
    confirm(&fixture.db, 2).await;
    assert_eq!(fixture.check().await, Ack::Fail("cancelled".into()));
    assert!(fixture.kept("sx").await, "mined: its fee chain history");
    assert!(!fixture.kept("s").await);
    assert_eq!(
        fixture.holds().await.1,
        Decimal::from(8),
        "back to the full balance less the fee mined"
    );
    assert_eq!(fixture.pending(), (0, 0), "no pending row, no pending leg");
    assert!(fixture.looked_for().await.is_empty(), "every index deleted");
    assert_eq!(
        fixture.ended().await,
        (
            Some(Outcome::Cancelled),
            Some("cancelled".into()),
            vec!["s".into(), "sx".into()]
        )
    );
}

#[tokio::test]
async fn a_payment_landing_while_its_cancel_is_in_flight_is_done() {
    let fixture = cancelled().await;
    let network = network();
    let legs = send(&network, "alice", "bob", 4, 1, coin(&network, "f", 0));
    fixture.mine(vec![mined(&network, "s", 0, legs)]).await;
    confirm(&fixture.db, 2).await;
    assert_eq!(fixture.check().await, Ack::Done);
    assert!(!fixture.kept("sx").await, "the cancel, pending, deleted");
    assert_eq!(fixture.holds().await, (0, Decimal::from(5)));
    assert_eq!(fixture.pending(), (0, 0));
    assert!(fixture.looked_for().await.is_empty());
    assert_eq!(fixture.ended().await.0, Some(Outcome::Final));
}

#[tokio::test]
async fn a_payment_mined_above_the_final_block_waits_and_a_reorg_sends_it_again_held() {
    let fixture = Fixture::new().await;
    let network = network();
    let legs = send(&network, "alice", "bob", 4, 1, coin(&network, "f", 0));
    fixture
        .mine(vec![mined(&network, "s", 0, legs.clone())])
        .await;
    assert!(matches!(fixture.check().await, Ack::Again(_, None)));
    assert!(fixture.sent().is_empty(), "mined, not final: waits");
    assert_eq!(fixture.holds().await, (0, Decimal::from(5)));

    BlockStore::delete(&fixture.storage, &network, 2)
        .await
        .unwrap();
    assert_eq!(
        rows(&fixture.directory, "s"),
        (vec![None], vec![None, None]),
        "pending again, its legs too"
    );
    assert_eq!(fixture.pending(), (1, 2), "its spend and fee held");
    assert_eq!(fixture.holds().await, (0, Decimal::from(5)), "unchanged");
    assert!(matches!(fixture.check().await, Ack::Again(_, None)));
    assert_eq!(fixture.sent(), [b"s".to_vec()], "the same bytes again");
    assert_eq!(fixture.holds().await, (0, Decimal::from(5)));

    // Mined in the new fork, then final.
    let fork = block(&network, 2, 1, vec![mined(&network, "s", 0, legs)]);
    BlockStore::insert(&fixture.storage, &run(&network, vec![fork]))
        .await
        .unwrap();
    assert_eq!(fixture.pending(), (0, 0));
    assert_eq!(fixture.held(), 2, "mined, its legs still held");
    assert!(matches!(fixture.check().await, Ack::Again(_, None)));
    confirm(&fixture.db, 2).await;
    assert_eq!(fixture.check().await, Ack::Done);
    assert_eq!(fixture.holds().await, (0, Decimal::from(5)), "spent once");
    assert_eq!(fixture.pending(), (0, 0));
    assert!(fixture.kept("s").await, "mined");
    assert!(fixture.looked_for().await.is_empty());
}

#[tokio::test]
async fn a_payment_no_block_includes_after_a_reorg_is_sent_again_then_bumped() {
    let fixture = Fixture::bumping(2, 2, None).await;
    let network = network();
    let legs = send(&network, "alice", "bob", 4, 1, coin(&network, "f", 0));
    fixture.mine(vec![mined(&network, "s", 0, legs)]).await;
    assert!(matches!(fixture.check().await, Ack::Again(_, None)));
    assert!(fixture.sent().is_empty(), "mined, not final: waits");

    BlockStore::delete(&fixture.storage, &network, 2)
        .await
        .unwrap();
    assert!(matches!(fixture.check().await, Ack::Again(_, None)));
    assert_eq!(
        fixture.sent(),
        [b"s".to_vec()],
        "sent again, not bumped yet"
    );
    assert_eq!(fixture.holds().await, (0, Decimal::from(5)), "held");

    // The new fork's block 2 holds nothing: `bump_after` read.
    let fork = block(&network, 2, 1, Vec::new());
    BlockStore::insert(&fixture.storage, &run(&network, vec![fork]))
        .await
        .unwrap();
    confirm(&fixture.db, 2).await;
    assert!(matches!(fixture.check().await, Ack::Again(_, None)));
    assert_eq!(fixture.sent(), [b"s".to_vec(), b"s+".to_vec()], "bumped");
    assert_eq!(
        fixture.holds().await,
        (0, Decimal::from(4)),
        "held once: 4 and the new fee, 2"
    );
    assert_eq!(
        fixture.progress().await,
        ("s+".to_owned(), vec!["s".to_owned()], 1)
    );
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
    assert!(fixture.kept("s").await);
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
    assert_eq!(fixture.holds().await, (0, Decimal::from(5)));
    assert_eq!(fixture.pending(), (0, 0));
    assert!(fixture.looked_for().await.is_empty(), "every index deleted");
}

#[tokio::test]
async fn a_replacement_every_node_refuses_is_undone() {
    let fixture = Fixture::bumping(1, 2, None).await;
    fixture
        .script
        .broadcasts
        .answer(Err(TransactionErrorKind::Rejected));
    let Ack::Again(_, Some(why)) = fixture.check().await else {
        panic!("checked again, with why");
    };
    assert!(why.starts_with("refused: "), "{why}");
    assert!(fixture.kept("s").await, "the old version current again");
    assert!(!fixture.kept("s+").await);
    assert_eq!(fixture.holds().await, (0, Decimal::from(5)));
    assert_eq!(fixture.looked_for().await, ["s"]);
    assert_eq!(fixture.progress().await, ("s".to_owned(), Vec::new(), 1));
}

#[tokio::test]
async fn an_expired_payment_is_dropped_signed_anew_and_its_new_version_found_final_is_done() {
    let fixture = Fixture::expired(2).await;
    assert!(
        matches!(fixture.check().await, Ack::Again(at, None) if at <= tasks::now()),
        "the new version is sent by the next run, at once"
    );
    assert!(fixture.sent().is_empty(), "recorded, not sent yet");
    assert!(!fixture.kept("s").await, "the expired one dropped");
    assert!(fixture.kept("s+").await);
    assert_eq!(
        fixture.holds().await,
        (0, Decimal::from(4)),
        "held once: 4 and the new fee, 2"
    );
    assert_eq!(
        watched(&fixture).await,
        [("s+".to_owned(), IndexState::Indexing)],
        "the expired one's index dropped"
    );
    assert_eq!(fixture.pending(), (1, 2), "only the new version pending");
    assert_eq!(
        fixture.progress().await,
        ("s+".to_owned(), vec!["s".to_owned()], 1)
    );
    assert!(matches!(fixture.check().await, Ack::Again(_, None)));
    assert_eq!(fixture.sent(), [b"s+".to_vec()], "the new version, sent");
    // Its index reads from block 2, where it lands.
    fixture.start("s+", 2).await;
    let network = network();
    let legs = send(&network, "alice", "bob", 4, 2, coin(&network, "f", 0));
    fixture.mine(vec![mined(&network, "s+", 0, legs)]).await;
    confirm(&fixture.db, 2).await;
    assert_eq!(fixture.check().await, Ack::Done);
    assert!(fixture.kept("s+").await, "mined");
    assert!(fixture.looked_for().await.is_empty(), "every index deleted");
}

#[tokio::test]
async fn an_expired_payment_at_max_bumps_ends_expired_and_leaves_nothing() {
    let fixture = Fixture::expired(2).await;
    fixture.save(&progress("s", &[], 2)).await;
    assert_eq!(fixture.check().await, Ack::Fail("expired".into()));
    assert!(fixture.sent().is_empty(), "not sent again");
    assert_eq!(
        fixture.holds().await,
        (1, Decimal::from(10)),
        "spend released"
    );
    assert_eq!(fixture.pending(), (0, 0), "no pending row, no pending leg");
    assert!(fixture.looked_for().await.is_empty(), "no index");
    assert_eq!(
        fixture.ended().await,
        (
            Some(Outcome::Expired),
            Some("expired".into()),
            vec!["s".into()]
        )
    );
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
