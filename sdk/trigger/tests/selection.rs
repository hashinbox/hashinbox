//! Which wallets: selectors over the fully indexed indexes, and the
//! targets `this`, `this` in another asset, and a selector.

mod common;

use base::{Address, Chain, Network};
use common::{World, id};
use serde_json::json;
use trigger::selection::{Index, Selection, Selector, Target};

/// 0xa (native, usdc) tagged deposit; 0xb native tagged deposit and hot;
/// 0xc native untagged; 0xd native deposit, behind.
fn world() -> World {
    let mut world = World::new();
    world
        .add("0xa", "native", (1, 1), &["deposit"])
        .add("0xa", "usdc", (1, 1), &["deposit"])
        .add("0xb", "native", (1, 1), &["deposit", "hot"])
        .add("0xc", "native", (1, 1), &[])
        .add_behind("0xd", "native", &["deposit"]);
    world
}

fn ids(pairs: &[(&str, &str)]) -> Vec<String> {
    pairs
        .iter()
        .map(|(address, asset)| id(address, asset))
        .collect()
}

#[test]
fn ids_pick_those_indexes() {
    let mut world = world();
    let picked = world.select(json!({ "ids": [id("0xc", "native"), id("0xa", "usdc")] }));
    assert_eq!(picked, ids(&[("0xa", "usdc"), ("0xc", "native")]), "by id");
}

#[test]
fn an_asset_picks_its_indexes() {
    let mut world = world();
    let picked = world.select(json!({ "asset": "native" }));
    assert_eq!(
        picked,
        ids(&[("0xa", "native"), ("0xb", "native"), ("0xc", "native")])
    );
}

#[test]
fn a_tag_picks_every_index_of_its_wallets() {
    let mut world = world();
    let picked = world.select(json!({ "tag": "deposit" }));
    assert_eq!(
        picked,
        ids(&[("0xa", "native"), ("0xa", "usdc"), ("0xb", "native")])
    );
}

#[test]
fn untagged_picks_wallets_without_any_tag() {
    let mut world = world();
    assert_eq!(
        world.select(json!({ "untagged": {} })),
        ids(&[("0xc", "native")])
    );
}

#[test]
fn all_picks_what_every_one_picks() {
    let mut world = world();
    let picked = world.select(json!({ "all": [{ "tag": "deposit" }, { "asset": "native" }] }));
    assert_eq!(picked, ids(&[("0xa", "native"), ("0xb", "native")]));
}

#[test]
fn any_picks_what_one_of_them_picks() {
    let mut world = world();
    let picked = world.select(json!({ "any": [{ "tag": "hot" }, { "untagged": {} }] }));
    assert_eq!(picked, ids(&[("0xb", "native"), ("0xc", "native")]));
}

#[test]
fn not_is_relative_to_the_fully_indexed() {
    let mut world = world();
    let picked = world.select(json!({ "not": { "tag": "deposit" } }));
    assert_eq!(
        picked,
        ids(&[("0xc", "native")]),
        "0xd is behind, never picked"
    );
}

#[test]
fn an_index_behind_is_never_picked() {
    let mut world = world();
    assert!(
        world
            .select(json!({ "ids": [id("0xd", "native")] }))
            .is_empty()
    );
}

#[test]
fn this_names_the_wallets_in_context() {
    let mut world = world();
    let this = ids(&[("0xa", "native"), ("0xc", "native")]);
    assert_eq!(world.target(json!({ "this": {} }), &this), Some(this));
}

#[test]
fn this_in_another_asset_names_the_same_addresses() {
    let mut world = world();
    let found = world.target(
        json!({ "this": { "asset": "usdc" } }),
        &ids(&[("0xa", "native")]),
    );
    assert_eq!(found, Some(ids(&[("0xa", "usdc")])));
}

#[test]
fn this_in_another_asset_is_unknown_when_one_address_lacks_it() {
    let mut world = world();
    let this = ids(&[("0xa", "native"), ("0xb", "native")]);
    assert_eq!(
        world.target(json!({ "this": { "asset": "usdc" } }), &this),
        None
    );
}

#[test]
fn this_in_another_asset_is_unknown_when_that_index_is_behind() {
    let mut world = world();
    world
        .add("0xe", "native", (1, 1), &[])
        .add_behind("0xe", "usdc", &[]);
    let found = world.target(
        json!({ "this": { "asset": "usdc" } }),
        &ids(&[("0xe", "native")]),
    );
    assert_eq!(found, None);
}

#[test]
fn a_selector_target_ignores_the_wallets_in_context() {
    let mut world = world();
    let found = world.target(
        json!({ "select": { "untagged": {} } }),
        &ids(&[("0xa", "native")]),
    );
    assert_eq!(found, Some(ids(&[("0xc", "native")])));
}

#[test]
fn a_selector_names_its_asset_at_its_top_or_under_a_top_all() {
    let asset = |json| {
        serde_json::from_value::<Selector>(json)
            .unwrap()
            .asset()
            .map(str::to_owned)
    };
    assert_eq!(asset(json!({ "asset": "usdc" })), Some("usdc".into()));
    assert_eq!(
        asset(json!({ "all": [{ "tag": "x" }, { "asset": "usdc" }] })),
        Some("usdc".into())
    );
    assert_eq!(asset(json!({ "any": [{ "asset": "usdc" }] })), None);
    let this: Target = serde_json::from_value(json!({ "this": { "asset": "usdc" } })).unwrap();
    assert_eq!(this.asset(), Some("usdc"));
}

fn sepolia_index(address: &str, confirmed: u64) -> Index {
    let network = Network::new(Chain::new("ethereum"), "sepolia");
    let mut one = Index::new(
        Address::new(network.clone(), address),
        base::Asset::native(network),
    );
    one.confirmed = confirmed.into();
    one
}

#[test]
fn an_index_id_is_chain_network_address_asset() {
    assert_eq!(sepolia_index("0xa", 0).id, id("0xa", "native"));
}

#[test]
fn spends_leave_out_indexes_holding_nothing() {
    let network = Network::new(Chain::new("ethereum"), "sepolia");
    let two = Selection::of(
        network.clone(),
        [sepolia_index("0xa", 5), sepolia_index("0xb", 0)],
    )
    .unwrap();
    let to = Address::new(network, "0xc");
    let spends = two.spends(Some(&to));
    assert_eq!(spends.len(), 1);
    assert_eq!(
        (spends[0].from.value.as_str(), spends[0].to.as_ref()),
        ("0xa", Some(&to))
    );
}

#[test]
fn a_selection_shares_an_address_only_when_all_have_it() {
    let network = Network::new(Chain::new("ethereum"), "sepolia");
    let two = Selection::of(
        network.clone(),
        [sepolia_index("0xa", 1), sepolia_index("0xb", 1)],
    )
    .unwrap();
    assert_eq!(two.address(), None);
    let one = two.only(&two[&id("0xa", "native")]).unwrap();
    assert_eq!(one.address().map(|a| a.value.as_str()), Some("0xa"));
}

#[test]
fn a_selection_refuses_an_index_of_another_network() {
    let devnet = Network::new(Chain::new("solana"), "devnet");
    let refused = Selection::of(devnet, [sepolia_index("0xa", 1)]).expect_err("refused");
    assert_eq!(refused.to_string(), "a wallet index is of another network");
}
