//! One contract for every queue driver, run against memory and SQLite.

use std::{sync::Arc, time::Duration};

use tasks::{
    Ack, Delivery, Executor, Handle, NewTask, Outcome, Policy, Runner, Settlement, Status, Store,
    StoreError, TaskResult,
    storage::{memory::Memory, sqlite::Sqlite},
};

async fn sqlite() -> (Sqlite, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.path().join("q.db"))
        .create_if_missing(true);
    let pool = sqlx::SqlitePool::connect_with(options).await.expect("pool");
    sqlx::raw_sql(include_str!("../../../schema/queue.sql"))
        .execute(&pool)
        .await
        .expect("schema");
    (Sqlite::new(pool), dir)
}

fn task(lane: &str, attempts: u32) -> NewTask {
    NewTask {
        policy: Policy {
            max_attempts: attempts,
            backoff: Duration::from_millis(10),
        },
        ..NewTask::new(lane, "kind", lane.as_bytes().to_vec())
    }
}

async fn finished(store: &dyn Store, id: &tasks::Id) -> TaskResult {
    match store.status(id).await.expect("status") {
        Some(Status::Finished(result)) => result,
        other => panic!("not finished: {other:?}"),
    }
}

fn finish(outcome: Outcome) -> Settlement {
    Settlement::Finish {
        outcome,
        output: Some(vec![7]),
        error: None,
    }
}

/// A lane runs its head only; lanes run concurrently; success is archived.
async fn lanes(store: &dyn Store) {
    let a1 = store.enqueue(task("a", 1), 0).await.expect("enqueue");
    let a2 = store.enqueue(task("a", 1), 0).await.expect("enqueue");
    let b1 = store.enqueue(task("b", 1), 0).await.expect("enqueue");
    let first = store.claim("w", 100, 1).await.expect("claim").expect("a1");
    assert_eq!((first.id.clone(), first.attempt), (a1.clone(), 1));
    let second = store.claim("w", 100, 1).await.expect("claim").expect("b1");
    assert_eq!(second.id, b1);
    assert!(
        store.claim("w", 100, 1).await.expect("claim").is_none(),
        "a2 waits for a1"
    );
    assert_eq!(
        store.status(&a1).await.expect("status"),
        Some(Status::Running)
    );
    assert!(matches!(
        store
            .settle(&a1, "other", finish(Outcome::Succeeded), 2)
            .await,
        Err(StoreError::Lost)
    ));
    store
        .settle(&a1, "w", finish(Outcome::Succeeded), 2)
        .await
        .expect("settle");
    let result = finished(store, &a1).await;
    assert_eq!(
        (
            result.outcome,
            result.output,
            result.attempts,
            result.finished_at
        ),
        (Outcome::Succeeded, Some(vec![7]), 1, 2)
    );
    assert_eq!(
        store
            .claim("w", 100, 3)
            .await
            .expect("claim")
            .expect("a2")
            .id,
        a2
    );
    assert!(matches!(
        store.settle(&a1, "w", finish(Outcome::Succeeded), 3).await,
        Err(StoreError::Lost)
    ));
}

/// Retry waits `backoff * attempts` and fails once attempts are exhausted.
async fn retries(store: &dyn Store) {
    let id = store.enqueue(task("r", 2), 0).await.expect("enqueue");
    store
        .claim("w", 100, 0)
        .await
        .expect("claim")
        .expect("first");
    store
        .settle(
            &id,
            "w",
            Settlement::Retry {
                error: "busy".into(),
            },
            50,
        )
        .await
        .expect("retry");
    assert_eq!(
        store.status(&id).await.expect("status"),
        Some(Status::Pending)
    );
    assert!(
        store.claim("w", 200, 59).await.expect("claim").is_none(),
        "not due before 60"
    );
    assert_eq!(
        store
            .claim("w", 200, 60)
            .await
            .expect("claim")
            .expect("second")
            .attempt,
        2
    );
    store
        .settle(
            &id,
            "w",
            Settlement::Retry {
                error: "still".into(),
            },
            70,
        )
        .await
        .expect("retry");
    let result = finished(store, &id).await;
    assert_eq!(
        (result.outcome, result.error.as_deref(), result.attempts),
        (Outcome::Failed, Some("still"), 2)
    );
}

/// A lapsed lease is reclaimed and counts; exhausted, it fails.
async fn leases(store: &dyn Store) {
    let id = store.enqueue(task("l", 2), 0).await.expect("enqueue");
    store
        .claim("dead", 10, 0)
        .await
        .expect("claim")
        .expect("first");
    assert!(
        store.claim("w", 100, 10).await.expect("claim").is_none(),
        "lease held at 10"
    );
    let again = store
        .claim("w", 100, 11)
        .await
        .expect("claim")
        .expect("reclaimed");
    assert_eq!(again.attempt, 2);
    assert!(matches!(
        store
            .settle(&id, "dead", finish(Outcome::Succeeded), 12)
            .await,
        Err(StoreError::Lost)
    ));
    assert!(store.claim("w", 200, 101).await.expect("claim").is_none());
    assert_eq!(finished(store, &id).await.outcome, Outcome::Failed);
}

/// Deadlines expire unrun heads; cancel takes pending tasks only.
async fn deadlines_and_cancel(store: &dyn Store) {
    let expiring = store
        .enqueue(
            NewTask {
                deadline: Some(5),
                ..task("d", 3)
            },
            0,
        )
        .await
        .expect("enqueue");
    let next = store.enqueue(task("d", 3), 0).await.expect("enqueue");
    assert_eq!(
        store
            .claim("w", 100, 5)
            .await
            .expect("claim")
            .expect("next")
            .id,
        next
    );
    assert_eq!(finished(store, &expiring).await.outcome, Outcome::Expired);
    assert!(
        !store.cancel(&next, 6).await.expect("cancel"),
        "running is not cancelled"
    );
    let pending = store.enqueue(task("c", 1), 0).await.expect("enqueue");
    assert!(store.cancel(&pending, 6).await.expect("cancel"));
    assert_eq!(finished(store, &pending).await.outcome, Outcome::Cancelled);
    assert!(matches!(
        store.enqueue(task("z", 0), 0).await,
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(
        store
            .status(&tasks::Id::from("unknown".to_owned()))
            .await
            .expect("status"),
        None
    );
}

async fn contract(store: &dyn Store) {
    lanes(store).await;
    retries(store).await;
    leases(store).await;
    deadlines_and_cancel(store).await;
}

#[tokio::test]
async fn memory_contract() {
    contract(&Memory::new()).await;
}

#[tokio::test]
async fn sqlite_contract() {
    let (store, _dir) = sqlite().await;
    contract(&store).await;
}

/// Acks by payload: "ok" succeeds, "no" rejects, "?" is ambiguous, else retry.
struct Script;

impl Executor for Script {
    fn execute(
        &self,
        delivery: Delivery,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Ack> + Send>> {
        Box::pin(async move {
            match delivery.payload.as_slice() {
                b"ok" => Ack::Done(Some(delivery.payload)),
                b"no" => Ack::Reject("refused".into()),
                b"?" => Ack::Ambiguous("timeout".into()),
                _ => Ack::Retry("again".into()),
            }
        })
    }
}

async fn runner(store: Arc<dyn Store>) {
    let runner = Runner::new(store.clone(), "w").executor("kind", Arc::new(Script));
    let mut handles = Vec::new();
    for (lane, payload) in [("1", "ok"), ("2", "no"), ("3", "?")] {
        let new = NewTask {
            payload: payload.as_bytes().to_vec(),
            ..task(lane, 3)
        };
        handles.push(Handle::new(
            store.enqueue(new, 0).await.expect("enqueue"),
            store.clone(),
        ));
    }
    let unknown = NewTask {
        kind: "nope".into(),
        ..task("4", 3)
    };
    handles.push(Handle::new(
        store.enqueue(unknown, 0).await.expect("enqueue"),
        store.clone(),
    ));
    assert_eq!(runner.pass().await.expect("pass"), 4);
    let mut outcomes = Vec::new();
    for handle in &handles {
        match handle
            .wait(Duration::from_millis(1), Duration::from_secs(1))
            .await
            .expect("wait")
        {
            Some(Status::Finished(result)) => outcomes.push(result.outcome),
            other => panic!("unfinished: {other:?}"),
        }
    }
    assert_eq!(
        outcomes,
        [
            Outcome::Succeeded,
            Outcome::Rejected,
            Outcome::Ambiguous,
            Outcome::Rejected
        ]
    );
}

#[tokio::test]
async fn memory_runner() {
    runner(Arc::new(Memory::new())).await;
}

#[tokio::test]
async fn sqlite_runner() {
    let (store, _dir) = sqlite().await;
    runner(Arc::new(store)).await;
}
