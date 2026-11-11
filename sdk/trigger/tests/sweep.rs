//! The UI's "Token sweep with gas funding" template, both triggers in one
//! round: `fund-gas` tops a token deposit's gas up to 1.25 × its fee,
//! `sweep-token` sends the token once the gas is there.

mod common;

use common::{Check, Filter, GAS, TOKEN, World, enabled, fund_gas, sent, sweep_token};
use trigger::{Decision, Enabled};

fn both() -> Vec<Enabled<Filter, Check>> {
    vec![
        enabled("fund-gas", fund_gas(), None),
        enabled("sweep-token", sweep_token(), None),
    ]
}

/// A deposit holding `token` of the token and `gas` (confirmed, pending)
/// native.
fn deposit(world: &mut World, address: &str, token: u64, gas: (u64, u64)) {
    world.add(address, "native", gas, &["deposit"]);
    world.add(address, TOKEN, (token, token), &["deposit"]);
}

fn round(world: &mut World) -> (Decision<common::Sent>, Decision<common::Sent>) {
    let mut decided = world.round(both());
    let sweep = decided.pop().unwrap();
    (decided.pop().unwrap(), sweep)
}

#[test]
fn a_deposit_with_the_token_and_no_gas_is_funded_with_headroom() {
    let mut world = World::new().with_gas();
    deposit(&mut world, "0xd1", 5_000, (0, 0));
    let (fund, sweep) = round(&mut world);
    // 1.25 × a fee of 10 is 12.5, rounded up.
    assert_eq!(fund.effects, [sent(GAS, "0xd1", "native", 13)]);
    assert!(sweep.effects.is_empty());
}

#[test]
fn the_headroom_follows_the_price_of_the_tokens_fee() {
    let mut world = World::new()
        .with_fees(|fees| fees.known_recipient(7.into()))
        .with_gas();
    deposit(&mut world, "0xd1", 5_000, (0, 0));
    let (fund, _) = round(&mut world);
    // 1.25 × 7 is 8.75, rounded up.
    assert_eq!(fund.effects, [sent(GAS, "0xd1", "native", 9)]);
}

#[test]
fn a_deposit_with_the_token_and_its_gas_is_swept() {
    let mut world = World::new().with_gas();
    deposit(&mut world, "0xd2", 2_000, (30, 30));
    let (fund, sweep) = round(&mut world);
    assert!(fund.effects.is_empty());
    assert_eq!(sweep.effects, [sent("0xd2", GAS, TOKEN, 2_000)]);
}

#[test]
fn gas_on_its_way_of_at_least_one_fee_is_not_topped_up_again() {
    let mut world = World::new().with_gas();
    deposit(&mut world, "0xd3", 100, (0, 10));
    let (fund, sweep) = round(&mut world);
    assert!(fund.effects.is_empty(), "no second top-up");
    assert!(
        sweep.effects.is_empty(),
        "not swept before the gas is confirmed"
    );
}

#[test]
fn gas_on_its_way_below_one_fee_is_topped_up_by_the_rest() {
    let mut world = World::new().with_gas();
    deposit(&mut world, "0xd4", 100, (0, 4));
    let (fund, _) = round(&mut world);
    assert_eq!(fund.effects, [sent(GAS, "0xd4", "native", 13 - 4)]);
}

#[test]
fn a_deposit_with_gas_exactly_one_fee_is_swept() {
    let mut world = World::new().with_gas();
    deposit(&mut world, "0xd5", 100, (10, 10));
    let (fund, sweep) = round(&mut world);
    assert!(fund.effects.is_empty());
    assert_eq!(sweep.effects, [sent("0xd5", GAS, TOKEN, 100)]);
}

#[test]
fn a_deposit_holding_nothing_is_neither_funded_nor_swept() {
    let mut world = World::new().with_gas();
    deposit(&mut world, "0xd6", 0, (0, 0));
    let (fund, sweep) = round(&mut world);
    assert!(fund.effects.is_empty() && sweep.effects.is_empty());
    assert!(world.fees.asked().is_empty(), "nothing to price");
}

#[test]
fn a_deposit_whose_token_index_is_behind_is_not_funded() {
    let mut world = World::new().with_gas();
    world.add("0xd7", "native", (0, 0), &["deposit"]);
    world.add_behind("0xd7", TOKEN, &["deposit"]);
    let (fund, sweep) = round(&mut world);
    assert_eq!(fund.last.stats.kept, 0);
    assert!(sweep.effects.is_empty());
}

#[test]
fn every_deposit_gets_one_of_the_two_at_most() {
    let mut world = World::new().with_gas();
    deposit(&mut world, "0xd1", 5_000, (0, 0));
    deposit(&mut world, "0xd2", 2_000, (30, 30));
    deposit(&mut world, "0xd3", 100, (0, 10));
    deposit(&mut world, "0xd4", 100, (0, 4));
    let (fund, sweep) = round(&mut world);
    let funded: Vec<_> = fund.effects.iter().map(|one| one.to.as_str()).collect();
    let swept: Vec<_> = sweep.effects.iter().map(|one| one.from.as_str()).collect();
    assert_eq!(funded, ["0xd1", "0xd4"]);
    assert_eq!(swept, ["0xd2"]);
    assert_eq!((fund.last.stats.selected, fund.last.stats.kept), (4, 2));
    assert_eq!((sweep.last.stats.selected, sweep.last.stats.kept), (4, 1));
}
