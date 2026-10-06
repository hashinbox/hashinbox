//! A cluster of scripted nodes: which node answers, when the primary moves,
//! and what a broadcast reaches. Every probe is a scripted `tip` call, so a
//! probe the test did not expect fails it.

use std::{num::NonZeroU64, sync::Arc, time::Duration};

use json_rpc::{
    BoxFuture, CallError, Client, Cluster, Error, ErrorKind, Failover, Limit, Policy, Request,
    Retry,
    testing::{Reply, Script},
};
use serde_json::json;

fn drive<T>(future: impl Future<Output = T>) -> T {
    futures_executor::block_on(future)
}

/// The injected tip probe: one `tip` call to the node.
fn tip<C: Client>(client: &C) -> BoxFuture<'_, Result<u64, CallError>> {
    Box::pin(async move {
        let raw = client.request(Request::new("tip", json!([]))).await??;
        Ok(raw.deserialize()?)
    })
}

fn height(tip: u64) -> Reply {
    Reply::ok("tip", json!(tip))
}

fn answer(name: &str) -> Reply {
    Reply::ok("answer", json!(name))
}

fn transport(kind: ErrorKind) -> Error {
    Error {
        kind,
        message: "hidden".to_owned(),
    }
}

fn timeout(method: &'static str) -> Reply {
    Reply::result(method, Err(transport(ErrorKind::Timeout)))
}

/// No refresh during a test unless the test asks for one.
fn policy(max_lag: u64, failure_threshold: u64, refresh: Duration) -> Policy {
    Policy {
        max_lag,
        failure_threshold: NonZeroU64::new(failure_threshold).expect("nonzero"),
        refresh,
        ..Policy::default()
    }
}

const NEVER: Duration = Duration::from_secs(3600);

fn cluster<C: Client + 'static>(nodes: Vec<C>, policy: Policy) -> Cluster<C> {
    drive(Cluster::new(nodes, tip::<C>, policy)).expect("nodes are not empty")
}

/// Written once against `Client`; the same argument takes one node or many.
async fn answered<C: Client>(client: &C) -> Result<String, CallError> {
    let raw = client.request(Request::new("answer", json!([]))).await??;
    Ok(raw.deserialize()?)
}

fn exhausted(scripts: &[&Script]) {
    for script in scripts {
        script.exhausted();
    }
}

#[test]
fn the_higher_tip_wins_at_construction_and_one_node_answers_alone() {
    let (a, b) = (
        Script::new([height(5)]),
        Script::new([height(9), answer("b")]),
    );
    let ranked = cluster(vec![a.clone(), b.clone()], Policy::default());
    assert_eq!(ranked.primary(), 1);
    assert_eq!(drive(answered(&ranked)).expect("b answers"), "b");
    assert_eq!(
        format!("{ranked:?}"),
        "Cluster { node_count: 2, primary: 1 }"
    );
    exhausted(&[&a, &b]);

    let only = Script::new([answer("only")]);
    let single = Cluster::one(only.clone());
    assert_eq!(drive(answered(&single)).expect("one node"), "only");
    assert_eq!(single.primary(), 0);
    assert_eq!(single.statistics()[0].snapshot().attempts, 1);
    only.exhausted();
}

#[test]
fn one_generic_caller_takes_a_bare_node_or_a_cluster_unchanged() {
    let bare = Script::new([answer("bare")]);
    assert_eq!(drive(answered(&bare)).expect("bare"), "bare");
    let one = Cluster::one(Script::new([answer("one")]));
    assert_eq!(drive(answered(&one)).expect("one"), "one");
    let many = cluster(
        vec![
            Script::new([height(1), answer("many")]),
            Script::new([height(1)]),
        ],
        Policy::default(),
    );
    assert_eq!(
        drive(answered(&Retry::new(Arc::new(many)))).expect("many"),
        "many"
    );
}

#[test]
fn a_failing_primary_yields_to_an_in_lag_node_and_can_win_again() {
    let a = Script::new([
        height(10),
        timeout("answer"),
        timeout("answer"),
        height(10),
        height(11),
        answer("a"),
    ]);
    let b = Script::new([
        height(9),
        height(9),
        answer("b"),
        timeout("answer"),
        timeout("answer"),
        height(11),
    ]);
    let c = Script::new([height(3), height(3), height(3)]);
    let nodes = cluster(vec![a.clone(), b.clone(), c.clone()], policy(2, 2, NEVER));
    assert_eq!(nodes.primary(), 0);
    for _ in 0..2 {
        assert!(drive(answered(&nodes)).is_err());
    }
    assert_eq!(drive(answered(&nodes)).expect("b took over"), "b");
    assert_eq!(nodes.primary(), 1);
    for _ in 0..2 {
        assert!(drive(answered(&nodes)).is_err());
    }
    assert_eq!(drive(answered(&nodes)).expect("a won again"), "a");
    assert_eq!(nodes.primary(), 0);
    exhausted(&[&a, &b, &c]);

    let a = Script::new([
        height(10),
        timeout("answer"),
        timeout("answer"),
        height(10),
        answer("a"),
    ]);
    let lagging = Script::new([height(3), height(3)]);
    let nodes = cluster(vec![a.clone(), lagging.clone()], policy(2, 2, NEVER));
    for _ in 0..2 {
        assert!(drive(answered(&nodes)).is_err());
    }
    assert_eq!(drive(answered(&nodes)).expect("no node in lag"), "a");
    exhausted(&[&a, &lagging]);

    let a = Script::new([
        height(10),
        Reply::failure("answer", -5, "not found"),
        answer("a"),
    ]);
    let b = Script::new([height(10)]);
    let nodes = cluster(vec![a.clone(), b.clone()], policy(2, 1, NEVER));
    let error = drive(answered(&nodes)).expect_err("an answered error");
    assert_eq!(error.remote_code(), Some(-5));
    assert_eq!(
        drive(answered(&nodes)).expect("an answer is no outage"),
        "a"
    );
    exhausted(&[&a, &b]);
}

#[test]
fn a_rate_limit_moves_the_primary_before_the_failure_threshold() {
    let limited = Error {
        kind: ErrorKind::HttpStatus(429),
        message: "slow down".to_owned(),
    };
    let a = Script::new([
        height(10),
        Reply::failure("answer", -32_601, "not a limit"),
        Reply::result("answer", Err(limited)),
        height(10),
        height(10),
        answer("a"),
    ]);
    let b = Script::new([
        height(10),
        height(10),
        Reply::failure("answer", -32_005, "limit exceeded"),
        height(10),
    ]);
    let nodes = cluster(vec![a.clone(), b.clone()], policy(0, 3, NEVER));
    assert!(drive(answered(&nodes)).is_err());
    assert_eq!(nodes.primary(), 0, "one ordinary failure is no trigger");
    assert!(drive(answered(&nodes)).is_err());
    let error = drive(answered(&nodes)).expect_err("b is limited too");
    assert_eq!(error.remote_code(), Some(-32_005));
    assert_eq!(nodes.primary(), 1);
    assert_eq!(drive(answered(&nodes)).expect("a again"), "a");
    exhausted(&[&a, &b]);
    assert!(
        Policy::default()
            .rate_limit_codes
            .contains(&Limit::Http(429))
    );
}

fn send() -> Request {
    Request::once("send", json!(["0x02"]))
}

fn sender(reply: Reply) -> Script {
    Script::new([height(1), reply]).once_only("send")
}

#[test]
fn a_broadcast_reaches_every_node_once_and_any_success_answers() {
    let nodes = [
        sender(timeout("send")),
        sender(Reply::ok("send", json!("txid"))),
        sender(Reply::failure("send", -26, "rejected")),
    ];
    let all = cluster(nodes.to_vec(), Policy::default());
    let accepted = drive(all.broadcast(send()))
        .expect("answered")
        .expect("success");
    assert_eq!(accepted.deserialize::<String>().expect("txid"), "txid");
    for node in &nodes {
        node.exhausted();
        assert_eq!(node.methods(), ["tip", "send"]);
    }

    let unknown = [
        sender(Reply::failure("send", -26, "rejected")),
        sender(timeout("send")),
        sender(Reply::failure("send", -25, "rejected")),
    ];
    let all = cluster(unknown.to_vec(), Policy::default());
    let error = drive(all.broadcast(send())).expect_err("one outcome is unknown");
    assert_eq!(error.kind, ErrorKind::Timeout);

    let refused = [
        sender(Reply::failure("send", -26, "rejected")),
        sender(Reply::failure("send", -25, "rejected")),
    ];
    let all = cluster(refused.to_vec(), Policy::default());
    let failure = drive(all.broadcast(send()))
        .expect("answered")
        .expect_err("refused");
    assert_eq!(failure.code, -25, "the last failure");
}

#[test]
fn a_forbidden_broadcast_is_never_repeated_at_any_node() {
    let nodes = [
        sender(Reply::result(
            "send",
            Err(transport(ErrorKind::Unavailable)),
        )),
        sender(Reply::result(
            "send",
            Err(transport(ErrorKind::HttpStatus(503))),
        )),
    ];
    let retried = nodes.iter().cloned().map(Retry::new).collect();
    // Every decorator forwards the broadcast; none narrows or repeats it.
    let shared = Arc::new(cluster(retried, Policy::default()));
    let decorated = Retry::new(Failover::new(vec![shared]).expect("one client"));
    let error = drive(decorated.broadcast(send())).expect_err("every node failed");
    assert!(error.is_retryable());
    for node in &nodes {
        node.exhausted();
        assert_eq!(node.methods(), ["tip", "send"]);
    }
}

#[test]
fn ties_keep_the_incumbent_and_only_a_due_refresh_probes() {
    let a = Script::new([height(5), height(7), height(7)]);
    let b = Script::new([height(7), height(7), answer("b"), height(7), answer("b")]);
    let refreshing = cluster(vec![a.clone(), b.clone()], policy(0, 3, Duration::ZERO));
    for _ in 0..2 {
        assert_eq!(drive(answered(&refreshing)).expect("b stays"), "b");
    }
    exhausted(&[&a, &b]);

    let a = Script::new([height(5)]);
    let b = Script::new([height(7), answer("b"), answer("b"), answer("b")]);
    let quiet = cluster(vec![a.clone(), b.clone()], policy(0, 3, NEVER));
    for _ in 0..3 {
        assert_eq!(drive(answered(&quiet)).expect("no probe"), "b");
    }
    exhausted(&[&a, &b]);

    let a = Script::new([height(5), height(9), answer("a")]);
    let b = Script::new([height(7), height(7)]);
    let diverged = cluster(vec![a.clone(), b.clone()], policy(0, 3, Duration::ZERO));
    assert_eq!(drive(answered(&diverged)).expect("a overtook b"), "a");
    exhausted(&[&a, &b]);
}
