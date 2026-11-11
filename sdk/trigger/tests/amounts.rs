//! Amounts in atomic units: values by decimals, balances confirmed or
//! pending, fees through the estimator, and arithmetic over them.

mod common;

use base::Decimal;
use common::{FEE, GAS, World, decimal, id};
use serde_json::json;

/// 0xa holds 500 confirmed and 300 pending native and 40 usdc; 0xb holds
/// nothing; 0xc and 0xd are recipients.
fn world() -> World {
    let mut world = World::new()
        .with_fees(|fees| fees.known_recipient(7.into()))
        .with_gas();
    world
        .add("0xa", "native", (500, 300), &["deposit"])
        .add("0xa", "usdc", (40, 40), &["deposit"])
        .add("0xb", "native", (0, 0), &["deposit"])
        .add("0xc", "native", (0, 0), &["out"])
        .add("0xd", "usdc", (0, 0), &["out"]);
    world
}

fn a() -> Vec<String> {
    vec![id("0xa", "native")]
}

fn both() -> Vec<String> {
    vec![id("0xa", "native"), id("0xb", "native")]
}

fn some(units: u64) -> Option<Decimal> {
    Some(Decimal::from(units))
}

#[test]
fn a_value_is_scaled_by_the_assets_decimals() {
    let mut world = world();
    let value = json!({ "value": "1.5" });
    assert_eq!(
        world.amount(value.clone(), &a(), "native").unwrap(),
        some(1_500)
    );
    assert_eq!(world.amount(value, &a(), "usdc").unwrap(), some(150));
}

#[test]
fn a_value_with_more_decimals_than_its_asset_is_an_error() {
    let mut world = world();
    assert!(
        world
            .amount(json!({ "value": "0.001" }), &a(), "usdc")
            .is_err()
    );
}

#[test]
fn a_value_in_an_unconfigured_asset_is_an_error() {
    let mut world = world();
    let refused = world
        .amount(json!({ "value": "1" }), &a(), "dai")
        .expect_err("refused");
    assert_eq!(refused.to_string(), "asset dai is not configured");
}

#[test]
fn a_balance_is_confirmed_spendable_by_default() {
    let mut world = world();
    assert_eq!(
        world
            .amount(json!({ "balance": {} }), &a(), "native")
            .unwrap(),
        some(500)
    );
}

#[test]
fn a_pending_balance_counts_what_is_on_its_way() {
    let mut world = world();
    let pending = json!({ "balance": { "of": { "this": {} }, "balance": "pending" } });
    assert_eq!(world.amount(pending, &a(), "native").unwrap(), some(300));
}

#[test]
fn a_balance_sums_every_wallet_it_names() {
    let mut world = world();
    let deposits = json!({ "balance": { "of": { "select": { "tag": "deposit" } } } });
    assert_eq!(
        world.amount(deposits, &a(), "native").unwrap(),
        some(540),
        "B2: assets mixed"
    );
}

#[test]
fn a_balance_in_another_asset_reads_the_same_address() {
    let mut world = world();
    let token = json!({ "balance": { "of": { "this": { "asset": "usdc" } } } });
    assert_eq!(world.amount(token, &a(), "usdc").unwrap(), some(40));
}

#[test]
fn a_balance_of_a_missing_sibling_is_unknown() {
    let mut world = world();
    let token = json!({ "balance": { "of": { "this": { "asset": "usdc" } } } });
    assert_eq!(world.amount(token, &both(), "usdc").unwrap(), None);
}

#[test]
fn a_fee_to_one_recipient_is_priced_as_that_recipient() {
    let mut world = world();
    let fee = json!({ "fee": { "to": { "select": { "ids": [id("0xc", "native")] } } } });
    assert_eq!(world.amount(fee, &a(), "native").unwrap(), some(7));
    assert_eq!(
        world.fees.asked()[0][0]
            .to
            .as_ref()
            .map(|to| to.value.as_str()),
        Some("0xc")
    );
}

#[test]
fn a_fee_to_one_address_of_several_indexes_is_priced_as_that_address() {
    let mut world = world();
    world.add("0xc", "usdc", (0, 0), &["out"]);
    let fee = json!({ "fee": { "to": { "select": { "any": [{ "ids": [id("0xc", "native")] }, { "ids": [id("0xc", "usdc")] }] } } } });
    assert_eq!(world.amount(fee, &a(), "native").unwrap(), some(7));
}

#[test]
fn a_fee_to_several_addresses_is_priced_as_a_new_address() {
    let mut world = world();
    let fee = json!({ "fee": { "to": { "select": { "tag": "out" } } } });
    assert_eq!(world.amount(fee, &a(), "native").unwrap(), some(FEE));
    assert_eq!(world.fees.asked()[0][0].to, None);
}

#[test]
fn a_fee_without_a_recipient_is_priced_as_a_new_address() {
    let mut world = world();
    assert_eq!(
        world.amount(json!({ "fee": {} }), &a(), "native").unwrap(),
        some(FEE)
    );
    assert_eq!(world.fees.asked()[0][0].to, None);
}

#[test]
fn a_fee_to_a_recipient_none_selects_is_priced_as_a_new_address() {
    let mut world = world();
    let fee = json!({ "fee": { "to": { "select": { "tag": "nobody" } } } });
    assert_eq!(world.amount(fee, &a(), "native").unwrap(), some(FEE));
}

#[test]
fn a_fee_to_a_missing_sibling_is_unknown() {
    let mut world = world();
    let fee = json!({ "fee": { "to": { "this": { "asset": "usdc" } } } });
    assert_eq!(world.amount(fee, &both(), "native").unwrap(), None);
    assert!(world.fees.asked().is_empty());
}

#[test]
fn a_fee_is_of_the_wallets_that_hold_something() {
    let mut world = world();
    assert_eq!(
        world
            .amount(json!({ "fee": {} }), &both(), "native")
            .unwrap(),
        some(FEE)
    );
    let asked = world.fees.asked();
    assert_eq!(asked[0].len(), 1);
    assert_eq!(asked[0][0].from.value, "0xa");
}

#[test]
fn a_fee_of_wallets_holding_nothing_is_zero_and_never_asked() {
    let mut world = world();
    let empty = [id("0xb", "native")];
    assert_eq!(
        world
            .amount(json!({ "fee": {} }), &empty, "native")
            .unwrap(),
        some(0)
    );
    assert!(world.fees.asked().is_empty());
}

#[test]
fn a_fee_of_another_asset_prices_that_asset() {
    let mut world = world();
    let fee = json!({ "fee": { "of": { "this": { "asset": "usdc" } }, "to": { "select": { "ids": [id(GAS, "native")] } } } });
    assert_eq!(world.amount(fee, &a(), "native").unwrap(), some(7));
    assert_eq!(world.fees.asked()[0][0].asset.id, "usdc");
}

#[test]
fn a_failing_estimate_is_an_error() {
    let mut world = World::new().with_fees(|fees| fees.failing("node unreachable"));
    world.add("0xa", "native", (500, 500), &[]);
    let refused = world
        .amount(json!({ "fee": {} }), &a(), "native")
        .expect_err("refused");
    assert_eq!(refused.to_string(), "node unreachable");
}

#[test]
fn times_rounds_up_to_a_whole_unit() {
    let mut world = world();
    for (value, factor, expected) in [
        ("0.007", "1.25", 9),
        ("0.008", "1.25", 10),
        ("0.001", "0.1", 1),
        ("0.003", "0.5", 2),
        ("0.004", "2", 8),
    ] {
        let times = json!({ "times": [{ "value": value }, factor] });
        let atomic = world.amount(times, &a(), "native").unwrap();
        assert_eq!(atomic, some(expected), "{value} × {factor}");
    }
}

#[test]
fn times_rounds_a_negative_amount_up_toward_zero() {
    let mut world = world();
    let negative = json!({ "times": [{ "sub": [{ "value": "0" }, { "value": "0.003" }] }, "0.5"] });
    assert_eq!(
        world.amount(negative, &a(), "native").unwrap(),
        Some(decimal("-1"))
    );
}

#[test]
fn times_of_an_unknown_amount_is_unknown() {
    let mut world = world();
    let unknown =
        json!({ "times": [{ "balance": { "of": { "this": { "asset": "usdc" } } } }, "2"] });
    assert_eq!(world.amount(unknown, &both(), "native").unwrap(), None);
}

#[test]
fn add_sub_min_and_max_combine_two_amounts() {
    let mut world = world();
    let (one, two) = (json!({ "value": "0.001" }), json!({ "value": "0.002" }));
    for (operation, expected) in [("add", "3"), ("sub", "-1"), ("min", "1"), ("max", "2")] {
        let amount = json!({ operation: [one.clone(), two.clone()] });
        assert_eq!(
            world.amount(amount, &a(), "native").unwrap(),
            Some(decimal(expected)),
            "{operation}"
        );
    }
}

#[test]
fn an_unknown_term_makes_the_whole_amount_unknown() {
    let mut world = world();
    let unknown = json!({ "balance": { "of": { "this": { "asset": "usdc" } } } });
    let max = json!({ "max": [unknown, { "value": "0" }] });
    assert_eq!(world.amount(max, &both(), "native").unwrap(), None);
}

#[test]
fn all_less_the_fee_is_the_balance_after_sending_it() {
    let mut world = world();
    let rest = json!({ "sub": [{ "balance": {} }, { "fee": {} }] });
    assert_eq!(world.amount(rest, &a(), "native").unwrap(), some(500 - FEE));
}
