//! The stored JSON: every form reads and writes back byte for byte, so rows
//! written today decode unchanged.

mod common;

use common::{
    Amount, Check, Definition, Filter, consolidate, forward, fund_gas, scheduled, sweep_token,
};
use serde::{Serialize, de::DeserializeOwned};
use trigger::{
    LastRound,
    actions::Call,
    conditions::Logic,
    selection::{Selector, Target},
};

/// Whether `stored` decodes as a `T` and encodes back to the same bytes.
fn round_trips<T: Serialize + DeserializeOwned>(stored: &str) {
    let decoded: T =
        serde_json::from_str(stored).unwrap_or_else(|error| panic!("{stored}: {error}"));
    assert_eq!(serde_json::to_string(&decoded).unwrap(), stored);
}

#[test]
fn every_selector_form_round_trips() {
    for stored in [
        r#"{"ids":["ethereum.sepolia.0xa.native"]}"#,
        r#"{"asset":"native"}"#,
        r#"{"tag":"deposit"}"#,
        r#"{"untagged":{}}"#,
        r#"{"all":[{"tag":"deposit"},{"asset":"native"}]}"#,
        r#"{"any":[{"tag":"hot"},{"untagged":{}}]}"#,
        r#"{"not":{"tag":"deposit"}}"#,
    ] {
        round_trips::<Selector>(stored);
    }
}

#[test]
fn every_target_form_round_trips() {
    for stored in [
        r#"{"this":{}}"#,
        r#"{"this":{"asset":"usdc"}}"#,
        r#"{"select":{"tag":"gas"}}"#,
    ] {
        round_trips::<Target>(stored);
    }
}

#[test]
fn every_amount_form_round_trips() {
    for stored in [
        r#"{"value":"1.25"}"#,
        r#"{"balance":{"of":{"this":{}}}}"#,
        r#"{"balance":{"of":{"this":{"asset":"usdc"}},"balance":"pending"}}"#,
        r#"{"fee":{"of":{"this":{}}}}"#,
        r#"{"fee":{"of":{"this":{"asset":"usdc"}},"to":{"select":{"ids":["x"]}}}}"#,
        r#"{"add":[{"value":"1"},{"value":"2"}]}"#,
        r#"{"sub":[{"balance":{"of":{"this":{}}}},{"fee":{"of":{"this":{}}}}]}"#,
        r#"{"min":[{"value":"1"},{"value":"2"}]}"#,
        r#"{"max":[{"sub":[{"times":[{"fee":{"of":{"this":{}}}},"1.25"]},{"value":"0"}]},{"value":"0"}]}"#,
        r#"{"times":[{"value":"3"},"0.5"]}"#,
    ] {
        round_trips::<Amount>(stored);
    }
}

#[test]
fn every_check_form_round_trips() {
    for stored in [
        r#"{"since":{"seconds":86400}}"#,
        r#"{"compare":{"of":{"this":{}},"aggregate":"sum","operator":"above","value":{"value":"0"}}}"#,
        r#"{"compare":{"of":{"this":{}},"aggregate":"min","balance":"pending","operator":"below","value":{"fee":{"of":{"this":{}}}}}}"#,
        r#"{"compare":{"of":{"select":{"tag":"x"}},"aggregate":"max","operator":"at_least","value":{"value":"1"}}}"#,
        r#"{"compare":{"of":{"this":{}},"aggregate":"count","operator":"at_most","value":{"value":"3"}}}"#,
        r#"{"compare":{"of":{"this":{}},"aggregate":"sum","operator":"equal","value":{"value":"2"}}}"#,
        r#"{"all":[]}"#,
        r#"{"any":[{"since":{"seconds":1}},{"not":{"since":{"seconds":2}}}]}"#,
    ] {
        round_trips::<Logic<Check>>(stored);
    }
}

#[test]
fn a_filter_round_trips() {
    let stored = r#"{"all":[{"compare":{"of":{"this":{"asset":"usdc"}},"aggregate":"sum","operator":"above","value":{"value":"0"}}}]}"#;
    round_trips::<Logic<Filter>>(stored);
}

#[test]
fn an_action_call_round_trips() {
    round_trips::<Call>(r#"{"name":"send","input":{"asset":"native"}}"#);
}

#[test]
fn a_trigger_round_trips_with_and_without_a_filter() {
    let call = r#"{"name":"send","input":{"source":{"this":{}}}}"#;
    let with = format!(
        r#"{{"selector":{{"tag":"deposit"}},"filter":{{"all":[]}},"check":{{"all":[]}},"action":{call}}}"#
    );
    round_trips::<Definition>(&with);
    let without = format!(
        r#"{{"selector":{{"tag":"deposit"}},"check":{{"since":{{"seconds":5}}}},"action":{call}}}"#
    );
    round_trips::<Definition>(&without);
}

#[test]
fn a_last_round_round_trips() {
    for stored in [
        r#"{"at":1,"confirmed":2,"selected":3,"kept":2,"checked":true,"jobs":2,"refused":null}"#,
        r#"{"at":1,"confirmed":2,"selected":3,"kept":0,"checked":false,"jobs":0,"refused":"asset dai is not configured"}"#,
    ] {
        round_trips::<LastRound>(stored);
    }
}

#[test]
fn every_template_the_ui_ships_round_trips() {
    for template in [
        forward(),
        consolidate(),
        fund_gas(),
        sweep_token(),
        scheduled(),
    ] {
        let trigger: Definition = serde_json::from_value(template.clone()).unwrap();
        assert_eq!(serde_json::to_value(&trigger).unwrap(), template);
    }
}

#[test]
fn defaults_are_written_out_as_the_server_writes_them_back() {
    let short: Amount = serde_json::from_str(r#"{"fee":{}}"#).unwrap();
    assert_eq!(
        serde_json::to_string(&short).unwrap(),
        r#"{"fee":{"of":{"this":{}}}}"#
    );
    let call: Call = serde_json::from_str(r#"{"name":"send"}"#).unwrap();
    assert_eq!(
        serde_json::to_string(&call).unwrap(),
        r#"{"name":"send","input":null}"#
    );
}

#[test]
fn a_call_with_an_unknown_field_does_not_decode() {
    assert!(serde_json::from_str::<Call>(r#"{"name":"send","extra":1}"#).is_err());
}
