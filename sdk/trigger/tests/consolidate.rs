//! The UI's "Consolidation over a threshold" template: once the deposits
//! worth spending hold at least 1 native together, each sends all of it
//! less its fee.

mod common;

use common::{GAS, World, consolidate, sent};

fn deposits(balances: &[(&str, u64, u64)]) -> World {
    let mut world = World::new().with_gas();
    for (address, confirmed, pending) in balances {
        world.add(address, "native", (*confirmed, *pending), &["deposit"]);
    }
    world
}

#[test]
fn deposits_below_the_threshold_together_wait() {
    let mut world = deposits(&[("0xd1", 400, 400), ("0xd2", 500, 500)]);
    let decided = world.decide(consolidate());
    assert!(decided.effects.is_empty());
    assert!(!decided.last.stats.checked);
    assert_eq!(decided.last.stats.kept, 2);
}

#[test]
fn deposits_at_the_threshold_together_each_send_all_less_their_fee() {
    let mut world = deposits(&[("0xd1", 400, 400), ("0xd2", 600, 600)]);
    let decided = world.decide(consolidate());
    let expected = [
        sent("0xd1", GAS, "native", 390),
        sent("0xd2", GAS, "native", 590),
    ];
    assert_eq!(decided.effects, expected);
}

#[test]
fn a_deposit_not_worth_its_fee_is_neither_counted_nor_sent() {
    let mut world = deposits(&[("0xd1", 400, 400), ("0xd2", 590, 590), ("0xd3", 10, 10)]);
    let decided = world.decide(consolidate());
    assert_eq!(decided.last.stats.kept, 2);
    assert!(!decided.last.stats.checked, "990 kept is below 1");
}

#[test]
fn pending_funds_do_not_count_toward_the_threshold() {
    let mut world = deposits(&[("0xd1", 400, 400), ("0xd2", 0, 2_000)]);
    let decided = world.decide(consolidate());
    assert_eq!(
        decided.last.stats.kept, 1,
        "0xd2 holds nothing spendable yet"
    );
    assert!(decided.effects.is_empty());
}
