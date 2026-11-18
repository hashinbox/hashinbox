//! A round paying two destinations from one source: one broadcast job per
//! payment, both kept in the round's one write (its run names them), run in
//! order, one transaction each.

use base::testing::{Caps, TestChain};
use serde_json::{Value, json};

use super::{App, decided_after, funded, indexed, runs};

pub(super) const NEEDS: Caps = Caps::NONE;

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let treasury = funded(app, chain).await;
    let (one, two) = (indexed(app, chain).await, indexed(app, chain).await);
    let network = chain.network().network;
    let native = |id: &str| format!("{id}.native");
    let body = json!({ "triggers": [{
        "id": "split",
        "chain": network.chain.as_str(),
        "network": network.name,
        "selector": {"ids": [native(&treasury.id)]},
        "check": {"compare": {
            "of": {"this": {}},
            "aggregate": "sum",
            "operator": "at_least",
            "value": {"value": "0.9"},
        }},
        "action": {"name": "send", "input": {
            "source": {"this": {}},
            "asset": "native",
            "destination": {"select": {"ids": [native(&one.id), native(&two.id)]}},
            "amount": {"value": "0.25"},
        }},
    }]});
    let (status, created) = app
        .api
        .call(axum::http::Method::POST, "/v1/triggers", Some(body))
        .await;
    assert_eq!(status, axum::http::StatusCode::CREATED, "{created}");

    let sends = decided_after(app, chain, &treasury).await;
    let ran = runs(app, "split").await;
    assert_eq!(ran.len(), 1, "{ran:?}");
    let ids: Vec<&Value> = sends.iter().map(|job| &job["id"]).collect();
    let named: Vec<&Value> = ran[0]["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|job| &job["id"])
        .collect();
    assert_eq!(named, ids, "the run names both jobs, in order");
    let mut paid: Vec<&str> = sends
        .iter()
        .map(|job| {
            assert!(job["txid"].is_string() && job["accepted"] == true, "{job}");
            job["input"]["to"].as_str().unwrap_or_default()
        })
        .collect();
    paid.sort_unstable();
    let mut payees = [one.address.as_str(), two.address.as_str()];
    payees.sort_unstable();
    assert_eq!(paid, payees, "one payment to each");
}
