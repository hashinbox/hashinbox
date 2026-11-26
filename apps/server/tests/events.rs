//! The message catalog (`logs.json`) against the event codes: every code
//! of every crate's `events` module has a template, every template is a
//! code's, and each `{placeholder}` is a field that code's event, or a span
//! it runs in, sends ([`FIELDS`], kept beside the call sites by hand). Then
//! how a template fills and how stdout shows an event.

use std::collections::{BTreeMap, BTreeSet};

use crate::events::{self, MESSAGES};

/// Every `events` module, as source: its `pub … &str = "code";` lines.
const SOURCES: [&str; 4] = [
    include_str!("../src/events.rs"),
    include_str!("../../../packages/rpc/src/events.rs"),
    include_str!("../../../packages/tasks/src/events.rs"),
    include_str!("../../../sdk/base/src/events.rs"),
];

/// The `job` span's fields (`tasks::Executor`).
const JOB: &[&str] = &["job_id", "kind", "topic", "attempt"];
/// The `node` span's fields (`usecase::nodes`).
const NODE: &[&str] = &["network", "node", "endpoint"];
/// The fields of a request's event (`rpc::Traced`).
const HTTP: &[&str] = &[
    "method",
    "batch",
    "ms",
    "wait_ms",
    "status",
    "rpc_errors",
    "error",
];
/// A payment's fields (`send.*`).
const PAYMENT: &[&str] = &["txid", "outcome", "versions", "reason", "network", "wallet"];
const BLOCK: &[&str] = &["position", "transactions", "matched", "legs"];
const SETTLE: &[&str] = &[
    "wallet",
    "asset",
    "position",
    "stored_total",
    "chain_balance",
    "adjustment",
];
const ROUND: &[&str] = &["evaluated", "fired", "skipped", "refused", "wallets", "ms"];
/// The `market` span's fields (`workers::market`).
const MARKET: &[&str] = &["market", "status"];

/// Each code, and the fields its event and its usual spans send.
const FIELDS: &[(&str, &[&[&str]])] = &[
    ("app.bootstrap", &[&["email", "settings"]]),
    ("app.db.created", &[&["path"]]),
    ("app.start", &[&["version", "networks", "bind"]]),
    ("app.stop", &[]),
    (
        "send.queued",
        &[&["network", "job_id", "wallet", "asset", "amount"]],
    ),
    ("send.sign.signed", &[PAYMENT, JOB]),
    ("send.sign.retried", &[PAYMENT, JOB]),
    ("send.sign.failed", &[PAYMENT, JOB]),
    ("send.broadcast.accepted", &[PAYMENT, JOB]),
    ("send.broadcast.refused", &[PAYMENT, JOB]),
    ("send.broadcast.unsure", &[PAYMENT, JOB]),
    ("send.cancel", &[PAYMENT, JOB]),
    ("send.bump", &[PAYMENT, JOB]),
    ("send.end.final", &[PAYMENT, JOB]),
    ("send.end.stopped", &[PAYMENT, JOB]),
    ("send.end.failed", &[PAYMENT, JOB]),
    ("endpoint.enabled", &[&["node", "reason"]]),
    ("endpoint.disabled", &[&["node", "kind", "reason"]]),
    ("endpoint.disable_failed", &[&["node", "error"]]),
    ("endpoint.uncounted", &[&["error"], NODE]),
    ("network.served", &[&["network"]]),
    ("network.unserved", &[&["network"]]),
    ("node.demoted", &[&["node", "reason", "streak"]]),
    ("node.restored", &[&["node"]]),
    ("rpc.failover", &[&["method", "from", "to", "reason"]]),
    ("index.block.matched", &[BLOCK]),
    ("index.block.unmatched", &[BLOCK]),
    (
        "index.leg",
        &[&[
            "wallet", "asset", "kind", "amount", "txid", "position", "total",
        ]],
    ),
    ("index.found", &[&["txid", "position", "reverted"]]),
    ("index.settle.unchanged", &[SETTLE]),
    ("index.settle.adjusted", &[SETTLE]),
    ("trigger.refused", &[&["trigger_id", "reason"]]),
    ("trigger.skipped", &[&["trigger_id", "reason"]]),
    (
        "trigger.fired",
        &[&["trigger_id", "wallet", "asset", "amount", "job_id"]],
    ),
    ("trigger.round.acted", &[ROUND]),
    ("trigger.round.idle", &[ROUND]),
    ("job.run", &[&["ms", "ack", "reason"], JOB]),
    ("rpc.http.answered", &[HTTP, NODE]),
    ("rpc.http.not_yet", &[HTTP, NODE]),
    ("rpc.http.failed", &[HTTP, NODE]),
    ("rpc.http.broken", &[HTTP, NODE]),
    ("rpc.slow", &[&["method", "waited_ms"], NODE]),
    ("rpc.retry", &[&["method", "attempt", "reason"]]),
    ("rpc.wait", &[&["calls", "wait_ms"]]),
    (
        "index.step",
        &[&[
            "start",
            "end",
            "blocks",
            "ms",
            "outcome",
            "calls",
            "rpc_ms",
            "wait_ms",
            "transactions",
            "matched",
            "legs",
            "lag",
        ]],
    ),
    ("index.reorg", &[&["position"]]),
    ("market.feed.answered", &[&["ms", "assets"], MARKET, JOB]),
    ("market.feed.failed", &[&["reason"], MARKET, JOB]),
    ("market.feed.recovered", &[MARKET, JOB]),
    ("market.feed.set", &[&["market", "feed"]]),
    ("auth.login.ok", &[&["email", "user", "method"]]),
    ("auth.login.failed", &[&["email", "method"]]),
    ("auth.recovery.created", &[&["user"]]),
    ("auth.recovery.replaced", &[&["user"]]),
    ("auth.recovery.removed", &[&["user"]]),
    ("auth.user.created", &[&["user", "email", "by"]]),
    ("auth.key.created", &[&["key", "user"]]),
    ("auth.key.revoked", &[&["key", "user"]]),
    ("auth.2fa.on", &[&["user"]]),
    ("auth.2fa.off", &[&["user"]]),
    ("auth.denied", &[&["scope"]]),
    ("setting.changed", &[&["key", "value"]]),
];

/// Every code the `events` modules declare.
fn codes() -> BTreeSet<&'static str> {
    let lines = SOURCES.iter().flat_map(|source| source.lines());
    let declared = lines.filter(|line| line.starts_with("pub") && line.contains(": &str = \""));
    declared.filter_map(|line| line.split('"').nth(1)).collect()
}

/// Each `{placeholder}` of `template`.
fn placeholders(template: &str) -> Vec<&str> {
    let opened = template.split('{').skip(1);
    opened
        .filter_map(|part| part.split_once('}'))
        .map(|(key, _)| key)
        .collect()
}

#[test]
fn every_code_has_a_template_and_every_template_a_code() {
    let catalog: BTreeSet<&str> = MESSAGES.keys().map(String::as_str).collect();
    let tabled: BTreeSet<&str> = FIELDS.iter().map(|(code, _)| *code).collect();
    assert_eq!(FIELDS.len(), tabled.len(), "each code once in FIELDS");
    assert_eq!(codes(), catalog, "the codes are the catalog's");
    assert_eq!(codes(), tabled, "the codes are FIELDS'");
}

#[test]
fn every_placeholder_is_a_field_its_event_sends() {
    let fields: BTreeMap<&str, Vec<&str>> = FIELDS
        .iter()
        .map(|(code, groups)| (*code, groups.concat()))
        .collect();
    for (code, template) in MESSAGES.iter() {
        assert_eq!(template.to_lowercase(), *template, "{code}: lower case");
        let sent = fields
            .get(code.as_str())
            .map(Vec::as_slice)
            .unwrap_or_default();
        for key in placeholders(template) {
            assert!(
                sent.contains(&key),
                "{code}: `{{{key}}}` is not a field it sends"
            );
        }
    }
}

#[test]
fn a_template_fills_from_fields_and_keeps_a_missing_key() {
    let fields = BTreeMap::from([("a".to_owned(), "1".to_owned())]);
    let (text, used) = events::fill("{a} and {b}", &fields);
    assert_eq!(text, "1 and {b}");
    assert_eq!(used, BTreeSet::from(["a"]));
}

#[test]
fn stdout_shows_the_filled_template_then_the_other_fields() {
    use std::{
        io,
        sync::{Arc, Mutex},
    };

    use tracing_subscriber::layer::SubscriberExt as _;

    /// The lines written, kept.
    #[derive(Clone, Default)]
    struct Written(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Written {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let written = Written::default();
    let into = written.clone();
    let stdout = tracing_subscriber::fmt::layer()
        .event_format(crate::usecase::logs::Line)
        .with_writer(move || into.clone());
    let subscriber = tracing_subscriber::registry().with(stdout);
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: events::NODE_DEMOTED, node = 3, reason = "http_429", streak = 2);
        tracing::warn!(target: "unknown.code", calls = 1);
        tracing::warn!(target: events::NODE_RESTORED, message = "its own", node = 3);
    });
    let written = String::from_utf8(written.0.lock().unwrap().clone()).unwrap();
    let lines: Vec<&str> = written
        .lines()
        .map(|line| line.split_once(' ').unwrap().1)
        .collect();
    assert_eq!(
        lines,
        [
            " INFO node.demoted: node 3 demoted: http_429 (streak 2)",
            " WARN unknown.code: calls=1",
            " WARN node.restored: its own node=3",
        ],
        "{written}"
    );
}
