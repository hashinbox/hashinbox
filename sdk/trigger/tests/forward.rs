//! The UI's "Native forward" template: each deposit holding more than twice
//! its fee forwards all of it less the fee to the chosen wallet.

mod common;

use common::{FEE, GAS, World, forward, sent};
use trigger::Stats;

fn deposits(balances: &[(&str, u64)]) -> World {
    let mut world = World::new().with_gas();
    for (address, balance) in balances {
        world.add(address, "native", (*balance, *balance), &["deposit"]);
    }
    world
}

#[test]
fn a_deposit_holding_more_than_twice_the_fee_forwards_all_but_the_fee() {
    let mut world = deposits(&[("0xd1", 500)]);
    let decided = world.decide(forward());
    assert_eq!(decided.effects, [sent("0xd1", GAS, "native", 500 - FEE)]);
}

#[test]
fn a_deposit_holding_exactly_twice_the_fee_stays() {
    let mut world = deposits(&[("0xd1", 2 * FEE)]);
    let decided = world.decide(forward());
    assert!(decided.effects.is_empty());
    assert_eq!(decided.last.stats.kept, 0);
}

#[test]
fn a_deposit_one_unit_over_twice_the_fee_forwards_the_rest() {
    let mut world = deposits(&[("0xd1", 2 * FEE + 1)]);
    let decided = world.decide(forward());
    assert_eq!(decided.effects, [sent("0xd1", GAS, "native", FEE + 1)]);
}

#[test]
fn each_deposit_forwards_on_its_own() {
    let mut world = deposits(&[("0xd1", 500), ("0xd2", 300), ("0xd3", 5)]);
    let decided = world.decide(forward());
    let expected = [
        sent("0xd1", GAS, "native", 490),
        sent("0xd2", GAS, "native", 290),
    ];
    assert_eq!(decided.effects, expected);
    let stats = Stats {
        selected: 3,
        kept: 2,
        checked: true,
        jobs: 2,
    };
    assert_eq!(decided.last.stats, stats);
}

#[test]
fn only_native_deposits_are_selected() {
    let mut world = deposits(&[("0xd1", 500)]);
    world.add("0xd1", "usdc", (500, 500), &["deposit"]);
    world.add("0xe1", "native", (500, 500), &[]);
    let decided = world.decide(forward());
    assert_eq!(decided.last.stats.selected, 1);
    assert_eq!(decided.effects.len(), 1);
}

#[test]
fn the_fee_is_priced_to_the_chosen_wallet() {
    let mut world = deposits(&[("0xd1", 500)]);
    world.decide(forward());
    let asked = world.fees.asked();
    assert!(!asked.is_empty());
    let to_gas = |spends: &base::Spends| {
        spends
            .iter()
            .all(|one| one.to.as_ref().is_some_and(|to| to.value == GAS))
    };
    assert!(asked.iter().all(to_gas));
}

#[test]
fn a_cheaper_known_recipient_leaves_more_to_forward() {
    let mut world = World::new()
        .with_fees(|fees| fees.known_recipient(4.into()))
        .with_gas();
    world.add("0xd1", "native", (500, 500), &["deposit"]);
    let decided = world.decide(forward());
    assert_eq!(decided.effects, [sent("0xd1", GAS, "native", 496)]);
}

#[test]
fn a_deposit_whose_key_is_not_kept_refuses_the_trigger() {
    let mut world = World::new().with_gas();
    world.add_watched("0xd1", "native", (500, 500), &["deposit"]);
    let decided = world.decide(forward());
    assert!(decided.effects.is_empty());
    assert_eq!(
        decided.last.refused.as_deref(),
        Some("source 0xd1 is not keyed here")
    );
}
