//! What may be stored: one test per error class, each with its message, and
//! every template the UI ships accepted.

mod common;

use common::{World, chosen, consolidate, forward, fund_gas, scheduled, sweep_token};
use serde_json::{Value, json};
use trigger::Class;

/// A valid trigger: deposits sending their balance to the chosen wallet.
fn valid() -> Value {
    json!({
        "selector": { "tag": "deposit" },
        "check": { "all": [] },
        "action": { "name": "send", "input": {
            "source": { "this": {} }, "asset": "native", "destination": chosen(),
            "amount": { "balance": {} },
        } },
    })
}

fn with(key: &str, value: Value) -> Value {
    let mut stored = valid();
    stored[key] = value;
    stored
}

fn refused(stored: Value) -> (Class, String) {
    let error = World::new().refusal(stored);
    (error.class, error.message().to_owned())
}

fn class(class: Class, message: &str) -> (Class, String) {
    (class, message.to_owned())
}

fn sum_above(of: Value, value: Value) -> Value {
    json!({ "compare": { "of": of, "aggregate": "sum", "operator": "above", "value": value } })
}

#[test]
fn every_template_the_ui_ships_is_accepted() {
    let world = World::new();
    for template in [
        forward(),
        consolidate(),
        fund_gas(),
        sweep_token(),
        scheduled(),
    ] {
        world.validate(template).expect("a template is valid");
    }
}

#[test]
fn an_empty_id_list_selects_nothing() {
    let expected = class(Class::EmptySelection, "a wallet list picks no wallet");
    assert_eq!(refused(with("selector", json!({ "ids": [] }))), expected);
}

#[test]
fn an_empty_any_of_wallets_selects_nothing() {
    let expected = class(
        Class::EmptySelection,
        "an empty \"any of\" selects no wallet",
    );
    assert_eq!(refused(with("selector", json!({ "any": [] }))), expected);
}

#[test]
fn an_empty_all_of_wallets_selects_everything() {
    let why = "an empty \"all of\" would select every wallet; name them by tag, asset or id";
    assert_eq!(
        refused(with("selector", json!({ "all": [] }))),
        class(Class::EmptySelection, why)
    );
}

#[test]
fn a_selector_of_only_nots_selects_everything_else() {
    let why = "a selector of only \"not\" would select every other wallet; name them by tag, \
               asset or id";
    for selector in [
        json!({ "not": { "tag": "deposit" } }),
        json!({ "any": [{ "not": { "tag": "deposit" } }, { "not": { "tag": "hot" } }] }),
        json!({ "all": [{ "not": { "tag": "deposit" } }] }),
    ] {
        let expected = class(Class::EmptySelection, why);
        assert_eq!(refused(with("selector", selector)), expected);
    }
    let bounded = json!({ "all": [{ "tag": "hot" }, { "not": { "tag": "deposit" } }] });
    World::new()
        .validate(with("selector", bounded))
        .expect("a not under a positive part is valid");
}

#[test]
fn an_empty_asset_selects_nothing() {
    let expected = class(Class::EmptySelection, "an asset is named");
    assert_eq!(refused(with("selector", json!({ "asset": "" }))), expected);
}

#[test]
fn an_empty_selection_nested_deep_is_found() {
    let nested = json!({ "all": [{ "tag": "deposit" }, { "not": { "ids": [] } }] });
    assert_eq!(refused(with("selector", nested)).0, Class::EmptySelection);
}

#[test]
fn a_target_selecting_nothing_is_refused_in_an_amount() {
    let filter = sum_above(
        json!({ "this": {} }),
        json!({ "fee": { "to": { "select": { "ids": [] } } } }),
    );
    assert_eq!(refused(with("filter", filter)).0, Class::EmptySelection);
}

#[test]
fn a_malformed_tag_is_refused() {
    let expected = class(Class::Malformed, "a tag is 1 to 64 of [a-z0-9-]");
    assert_eq!(
        refused(with("selector", json!({ "tag": "Not A Tag" }))),
        expected
    );
}

#[test]
fn a_check_that_can_never_hold_is_impossible() {
    let expected = class(Class::Impossible, "an empty \"any of\" never holds");
    assert_eq!(refused(with("check", json!({ "any": [] }))), expected);
}

#[test]
fn a_filter_that_can_never_hold_is_impossible() {
    assert_eq!(
        refused(with("filter", json!({ "any": [] }))).0,
        Class::Impossible
    );
}

#[test]
fn a_not_inside_a_not_is_redundant_in_a_selector() {
    let expected = class(Class::Redundant, "a \"not\" inside a \"not\" cancels out");
    assert_eq!(
        refused(with(
            "selector",
            json!({ "not": { "not": { "tag": "deposit" } } })
        )),
        expected
    );
}

#[test]
fn a_not_inside_a_not_is_redundant_in_a_check() {
    let check = json!({ "not": { "not": { "since": { "seconds": 60 } } } });
    assert_eq!(refused(with("check", check)).0, Class::Redundant);
}

#[test]
fn a_count_against_a_fraction_is_out_of_range() {
    let check = json!({ "compare": { "of": { "this": {} }, "aggregate": "count", "operator": "above", "value": { "value": "1.5" } } });
    let expected = class(Class::OutOfRange, "a count compares with a whole number");
    assert_eq!(refused(with("check", check)), expected);
}

#[test]
fn a_count_against_anything_but_a_value_is_out_of_range() {
    let check = json!({ "compare": { "of": { "this": {} }, "aggregate": "count", "operator": "above", "value": { "fee": {} } } });
    assert_eq!(refused(with("check", check)).0, Class::OutOfRange);
}

#[test]
fn a_factor_of_zero_or_less_is_out_of_range() {
    for factor in ["0", "-1"] {
        let filter = sum_above(
            json!({ "this": {} }),
            json!({ "times": [{ "fee": {} }, factor] }),
        );
        let expected = class(
            Class::OutOfRange,
            "an amount is multiplied by a positive factor",
        );
        assert_eq!(refused(with("filter", filter)), expected, "{factor}");
    }
}

#[test]
fn a_filter_that_never_reads_its_wallet_is_refused() {
    let elsewhere = json!({ "select": { "untagged": {} } });
    let filter = sum_above(elsewhere.clone(), json!({ "fee": { "of": elsewhere } }));
    let why = "a filter that never looks at this wallet keeps all or none; put it under When";
    assert_eq!(
        refused(with("filter", filter)),
        class(Class::FilterIgnoresWallet, why)
    );
}

#[test]
fn a_filter_reading_its_wallet_only_in_its_value_is_accepted() {
    let filter = sum_above(
        json!({ "select": { "untagged": {} } }),
        json!({ "add": [{ "fee": {} }, { "value": "1" }] }),
    );
    World::new()
        .validate(with("filter", filter))
        .expect("a fee of this wallet, nested, is enough");
}

#[test]
fn a_filter_mixing_one_that_reads_and_one_that_does_not_is_refused() {
    let reads = sum_above(json!({ "this": {} }), json!({ "value": "0" }));
    let ignores = sum_above(
        json!({ "select": { "untagged": {} } }),
        json!({ "value": "0" }),
    );
    let filter = json!({ "any": [reads, ignores] });
    assert_eq!(
        refused(with("filter", filter)).0,
        Class::FilterIgnoresWallet
    );
}

#[test]
fn an_unknown_action_is_refused() {
    let action = json!({ "name": "split", "input": {} });
    assert_eq!(
        refused(with("action", action)),
        class(Class::UnknownAction, "unknown action split")
    );
}

#[test]
fn an_action_input_that_does_not_decode_is_bad_input() {
    let action = json!({ "name": "send", "input": { "asset": "native" } });
    let (class, message) = refused(with("action", action));
    assert_eq!(class, Class::BadInput);
    assert!(
        message.starts_with("send input: missing field"),
        "{message}"
    );
}

#[test]
fn an_action_input_with_an_unknown_field_is_bad_input() {
    let mut stored = valid();
    stored["action"]["input"]["memo"] = json!("x");
    let (class, message) = refused(stored);
    assert_eq!(class, Class::BadInput);
    assert!(
        message.starts_with("send input: unknown field `memo`"),
        "{message}"
    );
}

#[test]
fn a_send_from_this_to_this_is_refused_by_the_action() {
    let mut stored = valid();
    stored["action"]["input"]["destination"] = json!({ "this": {} });
    let expected = class(Class::Refused, "a send from This to This is many to many");
    assert_eq!(refused(stored), expected);
}

#[test]
fn a_send_of_an_unconfigured_asset_is_not_configured() {
    let mut stored = valid();
    stored["action"]["input"]["asset"] = json!("dai");
    assert_eq!(
        refused(stored),
        class(Class::NotConfigured, "asset is not configured")
    );
}

#[test]
fn the_action_is_checked_before_the_selector() {
    let mut stored = with("selector", json!({ "ids": [] }));
    stored["action"]["name"] = json!("split");
    assert_eq!(refused(stored).0, Class::UnknownAction);
}

#[test]
fn the_selector_is_checked_before_the_filter_and_check() {
    let mut stored = with("selector", json!({ "ids": [] }));
    stored["check"] = json!({ "any": [] });
    assert_eq!(refused(stored).0, Class::EmptySelection);
}
