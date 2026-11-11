//! Conditions over the wallets in context: compares with every aggregate
//! and operator, counts, `since`, and `all`/`any`/`not` in written order,
//! stopping early.

mod common;

use common::{NOW, World, id};
use serde_json::{Value, json};

/// 0xa holds 100 confirmed (50 pending) native and 5 usdc; 0xb holds 300
/// (400 pending) native and no usdc index.
fn world() -> World {
    let mut world = World::new();
    world
        .add("0xa", "native", (100, 50), &[])
        .add("0xb", "native", (300, 400), &[])
        .add("0xa", "usdc", (5, 5), &[]);
    world
}

fn both() -> Vec<String> {
    vec![id("0xa", "native"), id("0xb", "native")]
}

fn compare(aggregate: &str, operator: &str, value: &str) -> Value {
    json!({ "compare": { "of": { "this": {} }, "aggregate": aggregate, "operator": operator, "value": { "value": value } } })
}

/// Holds over 0xa and 0xb.
fn holds(world: &mut World, check: Value) -> bool {
    world.holds(check, &both(), None).expect("it decides")
}

/// Holds; refers to an unconfigured asset, so it fails if it is ever evaluated.
fn trap() -> Value {
    json!({ "compare": { "of": { "select": { "asset": "dai" } }, "aggregate": "sum", "operator": "above", "value": { "value": "1" } } })
}

fn yes() -> Value {
    compare("count", "equal", "2")
}

fn no() -> Value {
    compare("count", "equal", "0")
}

#[test]
fn a_sum_compares_with_every_operator() {
    let mut world = world();
    for (operator, value, expected) in [
        ("above", "0.399", true),
        ("above", "0.4", false),
        ("at_least", "0.4", true),
        ("below", "0.401", true),
        ("below", "0.4", false),
        ("at_most", "0.4", true),
        ("equal", "0.4", true),
        ("equal", "0.401", false),
    ] {
        assert_eq!(
            holds(&mut world, compare("sum", operator, value)),
            expected,
            "sum {operator} {value}"
        );
    }
}

#[test]
fn a_min_compares_the_smallest_balance() {
    let mut world = world();
    assert!(holds(&mut world, compare("min", "equal", "0.1")));
    assert!(!holds(&mut world, compare("min", "above", "0.1")));
}

#[test]
fn a_max_compares_the_largest_balance() {
    let mut world = world();
    assert!(holds(&mut world, compare("max", "equal", "0.3")));
    assert!(holds(&mut world, compare("max", "at_most", "0.3")));
}

#[test]
fn a_count_compares_how_many_with_a_whole_number() {
    let mut world = world();
    assert!(holds(&mut world, compare("count", "equal", "2")));
    assert!(holds(&mut world, compare("count", "above", "1")));
    assert!(!holds(&mut world, compare("count", "below", "2")));
}

#[test]
fn a_count_against_a_fraction_is_refused_when_deciding() {
    let mut world = world();
    let refused = world
        .holds(compare("count", "above", "1.5"), &both(), None)
        .expect_err("refused");
    assert_eq!(refused.to_string(), "a count compares with a whole number");
}

#[test]
fn a_count_against_anything_but_a_value_is_refused_when_deciding() {
    let mut world = world();
    let fee = json!({ "compare": { "of": { "this": {} }, "aggregate": "count", "operator": "above", "value": { "fee": {} } } });
    let refused = world.holds(fee, &both(), None).expect_err("refused");
    assert_eq!(refused.to_string(), "a count compares with a value");
}

#[test]
fn a_pending_compare_reads_what_is_on_its_way() {
    let mut world = world();
    let pending = json!({ "compare": { "of": { "this": {} }, "aggregate": "sum", "balance": "pending", "operator": "equal", "value": { "value": "0.45" } } });
    assert!(holds(&mut world, pending));
}

#[test]
fn a_compare_in_another_asset_reads_the_siblings() {
    let mut world = world();
    let token = json!({ "compare": { "of": { "this": { "asset": "usdc" } }, "aggregate": "sum", "operator": "equal", "value": { "value": "0.05" } } });
    assert!(
        world
            .holds(token.clone(), &[id("0xa", "native")], None)
            .unwrap()
    );
    assert!(!holds(&mut world, token), "0xb has no usdc index: unknown");
}

#[test]
fn a_min_or_max_of_no_wallet_is_false() {
    let mut world = world();
    let nobody = |aggregate: &str| json!({ "compare": { "of": { "select": { "tag": "nobody" } }, "aggregate": aggregate, "operator": "at_least", "value": { "value": "0" } } });
    assert!(!holds(&mut world, nobody("min")));
    assert!(!holds(&mut world, nobody("max")));
    assert!(holds(&mut world, nobody("sum")), "a sum of none is 0");
}

#[test]
fn a_compare_with_no_asset_to_read_in_is_false() {
    let mut world = world();
    let nobody = json!({ "compare": { "of": { "select": { "tag": "nobody" } }, "aggregate": "sum", "operator": "at_least", "value": { "value": "0" } } });
    assert!(!world.holds(nobody, &[], None).unwrap());
}

#[test]
fn an_unknown_value_is_false() {
    let mut world = world();
    let unknown = json!({ "compare": { "of": { "this": {} }, "aggregate": "sum", "operator": "at_least", "value": { "balance": { "of": { "this": { "asset": "usdc" } } } } } });
    assert!(!holds(&mut world, unknown));
}

#[test]
fn all_stops_at_the_first_that_does_not_hold() {
    let mut world = world();
    assert!(!holds(&mut world, json!({ "all": [no(), trap()] })));
}

#[test]
fn any_stops_at_the_first_that_holds() {
    let mut world = world();
    assert!(holds(&mut world, json!({ "any": [yes(), trap()] })));
}

#[test]
fn a_condition_before_the_one_that_decides_is_evaluated() {
    let mut world = world();
    assert!(
        world
            .holds(json!({ "all": [trap(), no()] }), &both(), None)
            .is_err()
    );
    assert!(
        world
            .holds(json!({ "any": [trap(), yes()] }), &both(), None)
            .is_err()
    );
}

#[test]
fn a_fee_after_the_deciding_condition_is_never_asked() {
    let mut world = world();
    let fee = json!({ "compare": { "of": { "this": {} }, "aggregate": "sum", "operator": "above", "value": { "fee": {} } } });
    assert!(holds(&mut world, json!({ "any": [yes(), fee.clone()] })));
    assert!(!holds(&mut world, json!({ "all": [no(), fee.clone()] })));
    assert!(world.fees.asked().is_empty());
    assert!(holds(&mut world, json!({ "all": [yes(), fee] })));
    assert_eq!(world.fees.asked().len(), 1);
}

#[test]
fn not_holds_when_its_condition_does_not() {
    let mut world = world();
    assert!(holds(&mut world, json!({ "not": no() })));
    assert!(!holds(&mut world, json!({ "not": yes() })));
}

#[test]
fn an_error_under_not_stays_an_error() {
    let mut world = world();
    let refused = world
        .holds(json!({ "not": trap() }), &both(), None)
        .expect_err("refused");
    assert_eq!(refused.to_string(), "asset dai is not configured");
}

#[test]
fn an_empty_all_holds_and_an_empty_any_does_not() {
    let mut world = world();
    assert!(holds(&mut world, json!({ "all": [] })));
    assert!(!holds(&mut world, json!({ "any": [] })));
}

#[test]
fn combinators_nest() {
    let mut world = world();
    let nested =
        json!({ "all": [{ "any": [no(), { "not": no() }] }, { "not": { "all": [yes(), no()] } }] });
    assert!(holds(&mut world, nested));
}

#[test]
fn since_holds_for_a_trigger_never_run() {
    let mut world = world();
    assert!(
        world
            .holds(json!({ "since": { "seconds": 60 } }), &both(), None)
            .unwrap()
    );
}

#[test]
fn since_waits_until_the_seconds_have_passed_since_the_last_run() {
    let mut world = world();
    let minute = json!({ "since": { "seconds": 60 } });
    assert!(
        !world
            .holds(minute.clone(), &both(), Some(NOW - 59_999))
            .unwrap()
    );
    assert!(world.holds(minute, &both(), Some(NOW - 60_000)).unwrap());
}

#[test]
fn since_waits_for_a_last_run_in_the_future() {
    let mut world = world();
    assert!(
        !world
            .holds(json!({ "since": { "seconds": 1 } }), &both(), Some(NOW + 1))
            .unwrap()
    );
    assert!(
        world
            .holds(json!({ "since": { "seconds": 0 } }), &both(), Some(NOW + 1))
            .unwrap()
    );
}

#[test]
fn since_beyond_the_range_of_milliseconds_is_refused_when_deciding() {
    let mut world = world();
    let forever = json!({ "since": { "seconds": u64::MAX } });
    let refused = world
        .holds(forever, &both(), Some(NOW))
        .expect_err("refused");
    assert_eq!(refused.to_string(), "a wait is out of range");
}
