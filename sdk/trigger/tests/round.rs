//! A round: every enabled trigger at one confirmed block, in the order
//! given, each refusal its own, a last round for every trigger, and the
//! store read once per round.

mod common;

use common::{CONFIRMED, GAS, NOW, World, chosen, enabled, forward, scheduled, sent};
use serde_json::{Value, json};
use trigger::{LastRound, Stats};

fn deposits() -> World {
    let mut world = World::new().with_gas();
    world
        .add("0xd1", "native", (500, 500), &["deposit"])
        .add("0xd2", "native", (300, 300), &["deposit"])
        .add("0xd3", "native", (5, 5), &["deposit"]);
    world
}

/// Sends all of each deposit's balance to the gas wallet, checked by `check`.
fn sweep_when(check: Value) -> Value {
    json!({
        "selector": { "tag": "deposit" },
        "check": check,
        "action": { "name": "send", "input": {
            "source": { "this": {} }, "asset": "native", "destination": chosen(),
            "amount": { "balance": {} },
        } },
    })
}

fn refused_by_dai() -> Value {
    sweep_when(
        json!({ "compare": { "of": { "select": { "asset": "dai" } }, "aggregate": "sum", "operator": "above", "value": { "value": "1" } } }),
    )
}

#[test]
fn triggers_decide_in_the_order_given() {
    let mut world = deposits();
    let decided = world.round(vec![
        enabled("zeta", forward(), None),
        enabled("alpha", forward(), None),
    ]);
    let ids: Vec<_> = decided.iter().map(|one| one.trigger.as_str()).collect();
    assert_eq!(ids, ["zeta", "alpha"]);
}

#[test]
fn a_round_without_triggers_decides_nothing() {
    let mut world = deposits();
    assert!(world.round(Vec::new()).is_empty());
    assert_eq!(world.wallets.reads(), 0);
}

#[test]
fn a_refused_trigger_does_not_stop_the_others() {
    let mut world = deposits();
    let decided = world.round(vec![
        enabled("before", forward(), None),
        enabled("refused", refused_by_dai(), None),
        enabled("after", forward(), None),
    ]);
    assert_eq!(decided[0].effects, decided[2].effects);
    assert_eq!(decided[2].effects.len(), 2);
    assert_eq!(
        decided[1].last.refused.as_deref(),
        Some("asset dai is not configured")
    );
    assert!(decided[1].effects.is_empty());
}

#[test]
fn a_refusal_keeps_the_stats_counted_before_it() {
    let mut world = deposits();
    let decided = world.round(vec![enabled("refused", refused_by_dai(), None)]);
    let stats = Stats {
        selected: 3,
        kept: 3,
        checked: false,
        jobs: 0,
    };
    assert_eq!(decided[0].last.stats, stats);
}

#[test]
fn every_trigger_gets_its_last_round_acted_or_not() {
    let mut world = deposits();
    let decided = world.round(vec![
        enabled("acts", forward(), None),
        enabled("waits", sweep_when(json!({ "any": [] })), None),
    ]);
    let acted = LastRound {
        at: NOW,
        confirmed: CONFIRMED,
        stats: Stats {
            selected: 3,
            kept: 2,
            checked: true,
            jobs: 2,
        },
        refused: None,
    };
    assert_eq!(decided[0].last, acted);
    let waited = LastRound {
        stats: Stats {
            selected: 3,
            kept: 3,
            checked: false,
            jobs: 0,
        },
        ..acted
    };
    assert_eq!(decided[1].last, waited);
}

#[test]
fn jobs_count_the_effects_returned() {
    let mut world = deposits();
    let decided = world.round(vec![enabled("all", sweep_when(json!({ "all": [] })), None)]);
    assert_eq!(decided[0].effects.len(), 3);
    assert_eq!(decided[0].last.stats.jobs, 3);
}

#[test]
fn a_transfer_of_nothing_is_skipped() {
    let mut world = deposits();
    world.add("0xd4", "native", (0, 0), &["deposit"]);
    let decided = world.round(vec![enabled("all", sweep_when(json!({ "all": [] })), None)]);
    assert_eq!(decided[0].last.stats.selected, 4);
    assert_eq!(decided[0].last.stats.jobs, 3);
}

#[test]
fn each_selector_is_read_once_per_round() {
    let mut world = deposits();
    world.round(vec![
        enabled("one", forward(), None),
        enabled("two", forward(), None),
        enabled("three", forward(), None),
    ]);
    // The deposits, and the chosen wallet every fee is priced to.
    assert_eq!(world.wallets.reads(), 2);
}

#[test]
fn a_new_round_reads_again() {
    let mut world = deposits();
    world.round(vec![enabled("one", forward(), None)]);
    world.round(vec![enabled("one", forward(), None)]);
    assert_eq!(world.wallets.reads(), 4);
}

#[test]
fn each_trigger_waits_on_its_own_last_run() {
    let mut world = World::new().with_gas();
    world.add("0xt1", "native", (5_000, 5_000), &["treasury"]);
    let decided = world.round(vec![
        enabled("ran-today", scheduled(), Some(NOW - 1_000)),
        enabled("ran-yesterday", scheduled(), Some(NOW - 86_400_000)),
        enabled("never-ran", scheduled(), None),
    ]);
    let acted: Vec<_> = decided.iter().map(|one| !one.effects.is_empty()).collect();
    assert_eq!(acted, [false, true, true]);
}

#[test]
fn a_failing_fee_estimate_refuses_only_the_triggers_that_price_a_fee() {
    let mut world = World::new()
        .with_fees(|fees| fees.failing("node unreachable"))
        .with_gas();
    world.add("0xd1", "native", (500, 500), &["deposit"]);
    let decided = world.round(vec![
        enabled("prices", forward(), None),
        enabled("does-not", sweep_when(json!({ "all": [] })), None),
    ]);
    assert_eq!(decided[0].last.refused.as_deref(), Some("node unreachable"));
    assert_eq!(decided[1].effects, [sent("0xd1", GAS, "native", 500)]);
}

#[test]
fn an_unknown_action_is_refused_when_run() {
    let mut world = deposits();
    let mut stored = forward();
    stored["action"]["name"] = json!("split");
    let decided = world.round(vec![enabled("split", stored, None)]);
    assert_eq!(
        decided[0].last.refused.as_deref(),
        Some("unknown action split")
    );
    assert!(decided[0].last.stats.checked, "it got as far as acting");
}

#[test]
fn an_input_that_does_not_decode_is_refused_when_run() {
    let mut world = deposits();
    let mut stored = forward();
    stored["action"]["input"] = json!({ "asset": "native" });
    let decided = world.round(vec![enabled("bad", stored, None)]);
    let refused = decided[0].last.refused.clone().unwrap();
    assert!(
        refused.starts_with("send input: missing field"),
        "{refused}"
    );
}

#[test]
fn a_send_from_many_to_many_is_refused_when_run() {
    let mut world = deposits();
    let mut stored = sweep_when(json!({ "all": [] }));
    stored["action"]["input"]["destination"] = json!({ "select": { "tag": "deposit" } });
    let decided = world.round(vec![enabled("many", stored, None)]);
    let refused = decided[0].last.refused.as_deref();
    assert_eq!(
        refused,
        Some("a send has many sources and many destinations")
    );
}
