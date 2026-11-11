//! The UI's "Scheduled transfer" template: the treasury sends 0.1 native at
//! most once a day.

mod common;

use common::{GAS, NOW, World, enabled, scheduled, sent};
use trigger::Stats;

const DAY: u64 = 86_400_000;

fn treasury() -> World {
    let mut world = World::new().with_gas();
    world.add("0xt1", "native", (5_000, 5_000), &["treasury"]);
    world.add("0xd1", "native", (5_000, 5_000), &["deposit"]);
    world
}

#[test]
fn a_treasury_never_run_sends_now() {
    let mut world = treasury();
    let decided = world.decide(scheduled());
    assert_eq!(decided.effects, [sent("0xt1", GAS, "native", 100)]);
}

#[test]
fn a_treasury_run_an_hour_ago_waits() {
    let mut world = treasury();
    let decided = world.round(vec![enabled("daily", scheduled(), Some(NOW - 3_600_000))]);
    assert!(decided[0].effects.is_empty());
    let stats = Stats {
        selected: 1,
        kept: 1,
        checked: false,
        jobs: 0,
    };
    assert_eq!(decided[0].last.stats, stats);
}

#[test]
fn a_treasury_run_a_day_ago_sends_again() {
    let mut world = treasury();
    let decided = world.round(vec![enabled("daily", scheduled(), Some(NOW - DAY))]);
    assert_eq!(decided[0].effects, [sent("0xt1", GAS, "native", 100)]);
}

#[test]
fn a_treasury_run_a_moment_short_of_a_day_waits() {
    let mut world = treasury();
    let decided = world.round(vec![enabled("daily", scheduled(), Some(NOW - DAY + 1))]);
    assert!(decided[0].effects.is_empty());
}

#[test]
fn only_the_treasury_is_selected() {
    let mut world = treasury();
    let decided = world.decide(scheduled());
    assert_eq!(decided.last.stats.selected, 1);
}

#[test]
fn a_treasury_holding_less_than_the_amount_is_still_asked_to_send_it() {
    let mut world = World::new().with_gas();
    world.add("0xt1", "native", (50, 50), &["treasury"]);
    let decided = world.decide(scheduled());
    assert_eq!(decided.effects, [sent("0xt1", GAS, "native", 100)], "B13");
}
