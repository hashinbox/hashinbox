//! The store's rules, on `MemoryStore`: line order, uniqueness, what a
//! claim takes and leaves, skips, expiry, leases, and guarded updates.

use serde_json::json;
use tasks::{Admission, Claim, Id, Job, MemoryStore, OnLock, Status, Store, StoreError, Topic};

/// A job of `kind` on `topic`, due from time 0.
fn job(kind: &str, topic: &str) -> Job {
    Job::new(Topic::parse(topic).unwrap(), kind, json!(null)).run_at(0)
}

/// Enqueues `job`, not unique; its id.
async fn enqueue(store: &MemoryStore, job: Job) -> Id {
    match store.enqueue(job, false).await.unwrap() {
        Admission::Queued(id) => id,
        Admission::Duplicate(id) => panic!("a duplicate of {id}"),
    }
}

/// Claims up to `limit` jobs of `kind` at `now`, leased until `now + 100`;
/// their ids.
async fn claim(
    store: &MemoryStore,
    kind: &str,
    limit: usize,
    on_lock: OnLock,
    now: u64,
) -> Vec<Id> {
    let claim = Claim {
        kind,
        limit,
        on_lock,
        now,
        until: now + 100,
    };
    let jobs = store.claim(claim).await.unwrap();
    jobs.into_iter().map(|job| job.id).collect()
}

/// The stored job `id`.
async fn get(store: &MemoryStore, id: Id) -> Job {
    store.get(id).await.unwrap().unwrap()
}

/// Stores job `id` done, as its run would.
async fn finish(store: &MemoryStore, id: Id) {
    let running = get(store, id).await;
    let attempt = running.attempts;
    let done = Job {
        status: Status::Done,
        ..running
    };
    store.update(done, attempt).await.unwrap();
}

#[tokio::test]
async fn enqueue_numbers_jobs_in_order_and_unique_keeps_one_per_topic() {
    let store = MemoryStore::new();
    let first = enqueue(&store, job("k", "a")).await;
    let second = enqueue(&store, job("k", "b")).await;
    assert_eq!((first, second), (Id(1), Id(2)));
    assert_eq!(get(&store, first).await.input, json!(null));

    let held = store.enqueue(job("u", "n"), true).await.unwrap();
    assert_eq!(held, Admission::Queued(Id(3)));
    let again = store.enqueue(job("u", "n"), true).await.unwrap();
    assert_eq!(
        again,
        Admission::Duplicate(Id(3)),
        "one unfinished per topic"
    );
    let other_kind = store.enqueue(job("v", "n"), true).await.unwrap();
    assert_eq!(other_kind, Admission::Queued(Id(4)), "per kind");

    claim(&store, "u", 1, OnLock::Wait, 0).await;
    let running = store.enqueue(job("u", "n"), true).await.unwrap();
    assert_eq!(running, Admission::Duplicate(Id(3)), "while it runs");
    finish(&store, Id(3)).await;
    let after = store.enqueue(job("u", "n"), true).await.unwrap();
    assert_eq!(after, Admission::Queued(Id(5)), "once it finished");
}

#[tokio::test]
async fn claim_takes_due_jobs_of_its_kind_oldest_first_up_to_its_limit() {
    let store = MemoryStore::new();
    let ids = [
        enqueue(&store, job("k", "a")).await,
        enqueue(&store, job("k", "b")).await,
        enqueue(&store, job("other", "c")).await,
        enqueue(&store, job("k", "d").run_at(50)).await,
        enqueue(&store, job("k", "e")).await,
    ];
    assert_eq!(
        claim(&store, "k", 2, OnLock::Wait, 10).await,
        [ids[0], ids[1]]
    );
    assert_eq!(
        claim(&store, "k", 5, OnLock::Wait, 10).await,
        [ids[4]],
        "not before run_at"
    );
    assert_eq!(claim(&store, "k", 5, OnLock::Wait, 50).await, [ids[3]]);
    let claimed = get(&store, ids[0]).await;
    assert_eq!(
        (claimed.status, claimed.attempts, claimed.run_at),
        (Status::Running, 1, 110),
        "running, its attempt counted, leased"
    );
    assert_eq!(
        get(&store, ids[2]).await.status,
        Status::Pending,
        "another kind"
    );
}

#[tokio::test]
async fn nothing_starts_beside_or_ahead_of_a_conflicting_job() {
    let store = MemoryStore::new();
    let wallet = enqueue(&store, job("k", "n.w1")).await;
    let network = enqueue(&store, job("k", "n")).await;
    let again = enqueue(&store, job("k", "n.w1")).await;
    let other = enqueue(&store, job("k", "n.w2")).await;
    let elsewhere = enqueue(&store, job("k", "m")).await;

    // The wallet runs; the network waits for it, and everything on the
    // network waits behind the network.
    assert_eq!(
        claim(&store, "k", 9, OnLock::Wait, 0).await,
        [wallet, elsewhere]
    );
    assert!(claim(&store, "k", 9, OnLock::Wait, 0).await.is_empty());
    finish(&store, wallet).await;
    assert_eq!(claim(&store, "k", 9, OnLock::Wait, 0).await, [network]);
    finish(&store, network).await;
    assert_eq!(claim(&store, "k", 9, OnLock::Wait, 0).await, [again, other]);
}

#[tokio::test]
async fn a_job_waiting_for_a_fresh_run_holds_no_place_but_a_retry_does() {
    let store = MemoryStore::new();
    let later = enqueue(&store, job("k", "n").run_at(50)).await;
    let wallet = enqueue(&store, job("k", "n.w1")).await;
    assert_eq!(
        claim(&store, "k", 9, OnLock::Wait, 0).await,
        [wallet],
        "not due: no place"
    );
    finish(&store, wallet).await;

    let retried = Job {
        attempts: 1,
        ..get(&store, later).await
    };
    let store = MemoryStore::new();
    enqueue(&store, retried).await;
    let behind = enqueue(&store, job("k", "n.w1")).await;
    assert!(
        claim(&store, "k", 9, OnLock::Wait, 0).await.is_empty(),
        "a retry keeps its place: {behind} waits"
    );
}

#[tokio::test]
async fn a_skip_job_gives_way_to_a_newer_conflicting_job() {
    let store = MemoryStore::new();
    let held = enqueue(&store, job("send", "n.w1")).await;
    claim(&store, "send", 1, OnLock::Wait, 0).await;
    let round = enqueue(&store, job("round", "n")).await;
    assert!(
        claim(&store, "round", 1, OnLock::Skip, 0).await.is_empty(),
        "behind the running send"
    );
    assert_eq!(get(&store, round).await.status, Status::Pending);

    let newer = enqueue(&store, job("send", "n.w2")).await;
    assert!(claim(&store, "round", 1, OnLock::Skip, 0).await.is_empty());
    let skipped = get(&store, round).await;
    assert_eq!(
        (skipped.status, skipped.reason),
        (Status::Skipped, Some(format!("skipped: {newer}")))
    );
    finish(&store, held).await;
    assert_eq!(claim(&store, "send", 1, OnLock::Wait, 0).await, [newer]);
}

#[tokio::test]
async fn a_claim_expires_its_kind_past_their_deadline() {
    let store = MemoryStore::new();
    let late = enqueue(&store, job("k", "a").with_deadline(5)).await;
    let kept = enqueue(&store, job("k", "b").with_deadline(50)).await;
    let other = enqueue(&store, job("other", "c").with_deadline(5)).await;
    assert_eq!(claim(&store, "k", 9, OnLock::Wait, 10).await, [kept]);
    let expired = get(&store, late).await;
    assert_eq!(
        (expired.status, expired.reason.as_deref()),
        (Status::Failed, Some("expired"))
    );
    assert_eq!(
        get(&store, other).await.status,
        Status::Pending,
        "only its kind"
    );
}

#[tokio::test]
async fn a_run_whose_lease_ended_is_claimed_again() {
    let store = MemoryStore::new();
    let id = enqueue(&store, job("k", "n")).await;
    let behind = enqueue(&store, job("k", "n.w1")).await;
    assert_eq!(claim(&store, "k", 9, OnLock::Wait, 0).await, [id]);
    assert!(
        claim(&store, "k", 9, OnLock::Wait, 99).await.is_empty(),
        "leased"
    );
    assert_eq!(
        claim(&store, "k", 9, OnLock::Wait, 100).await,
        [id],
        "lease ended"
    );
    let again = get(&store, id).await;
    assert_eq!((again.status, again.attempts), (Status::Running, 2));
    assert_ne!(
        get(&store, behind).await.status,
        Status::Running,
        "still behind it"
    );
}

#[tokio::test]
async fn an_update_lands_only_while_its_run_holds_the_job() {
    let store = MemoryStore::new();
    let id = enqueue(&store, job("k", "a")).await;
    assert!(
        matches!(
            store.update(get(&store, id).await, 0).await,
            Err(StoreError::Stale)
        ),
        "not claimed"
    );
    claim(&store, "k", 1, OnLock::Wait, 0).await;
    let saved = Job {
        state: Some(json!({"sent": true})),
        ..get(&store, id).await
    };
    store.update(saved.clone(), 1).await.unwrap();
    assert_eq!(get(&store, id).await.state, Some(json!({"sent": true})));

    // A fresh run clears its attempts; the write still belongs to attempt 1.
    let done = Job {
        status: Status::Pending,
        attempts: 0,
        ..saved.clone()
    };
    store.update(done, 1).await.unwrap();
    assert!(
        matches!(store.update(saved, 1).await, Err(StoreError::Stale)),
        "a late save never undoes how the run ended"
    );
    assert_eq!(get(&store, id).await.status, Status::Pending);
}

#[tokio::test]
async fn an_update_from_an_older_attempt_is_stale() {
    let store = MemoryStore::new();
    let id = enqueue(&store, job("k", "a")).await;
    claim(&store, "k", 1, OnLock::Wait, 0).await;
    let first = get(&store, id).await;
    claim(&store, "k", 1, OnLock::Wait, 100).await;
    assert_eq!(get(&store, id).await.attempts, 2);
    assert!(matches!(
        store.update(first, 1).await,
        Err(StoreError::Stale)
    ));
}
