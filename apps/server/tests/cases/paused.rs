//! A pause stops tracking, not spending. A send from a paused index is
//! accepted and lands: once final its job is done and its transaction is
//! mined in its block, the sender's spend and fee and the indexed
//! recipient's receive kept. Resumed, the index settles with nothing counted
//! twice: the settle's adjustment is zero. A send from a wallet with no
//! index of the asset, or for a token none of `native` (the fee), is
//! refused (422): no job is queued.

use axum::http::{Method, StatusCode};
use base::testing::{Caps, TestChain};
use serde_json::{Value, json};

use super::{
    App, Wallet, accepted, at_tip, funded, indexed, number, quarter, reaches, sends_from, settle,
    succeeded, text, unit, until,
};

pub(super) const NEEDS: Caps = Caps::NONE;

pub(super) fn prepare<T: TestChain>(_chain: &T) {}

pub(super) async fn case<T: TestChain>(app: &App, chain: &T) {
    let network = chain.network();
    let token = network.assets[1].asset.clone();
    let wallet = funded(app, chain).await;
    let other = indexed(app, chain).await;

    let stranger = app.api.wallet(&network).await;
    let missing = format!(
        "not-indexed: the sender has no index {}.native",
        stranger.id
    );
    assert_eq!(
        refused(app, &stranger, "native", &other.address).await,
        missing
    );
    app.api.index(&stranger, &token, None).await;
    assert_eq!(
        refused(app, &stranger, &token, &other.address).await,
        missing
    );
    assert!(sends_from(app, &stranger).await.is_empty());

    set_state(app, &wallet, "paused").await;
    let job = app
        .api
        .send(&wallet, "native", &other.address, "0.25")
        .await;
    accepted(app, &job).await;
    settle(chain);
    let txid = succeeded(app, &job).await["txid"].clone();
    let mined = transaction(app, chain, &txid).await;
    assert!(mined["block"].is_string(), "{mined}");
    let quarter = quarter(chain);
    let legs: Vec<_> = mined["legs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|leg| {
            (
                text(leg, "wallet_index_id"),
                leg["kind"].as_i64(),
                text(leg, "amount"),
            )
        })
        .collect();
    let (sender, payee) = (
        format!("{}.native", wallet.id),
        format!("{}.native", other.id),
    );
    assert!(
        legs.contains(&(sender.clone(), Some(1), quarter.clone())),
        "{legs:?}"
    );
    assert!(
        legs.contains(&(payee, Some(0), quarter.clone())),
        "{legs:?}"
    );
    let fee = legs
        .iter()
        .find(|(index, kind, _)| *index == sender && *kind == Some(4))
        .map(|(_, _, amount)| number(amount))
        .expect("the sender's fee leg");
    let spent = app.api.legs(&wallet, "native").await;
    assert_eq!(spent[0]["transaction"], mined["id"], "{spent:?}");
    assert_eq!(spent[0]["block"], mined["block"], "{spent:?}");

    // Left behind the final block (a node's `finalized` trails its head by
    // six), resumed: it settles at the chain's balance, which already
    // counts the spend kept.
    chain.mine(10);
    at_tip(app, chain).await;
    set_state(app, &wallet, "indexing").await;
    let left = (number(&unit(chain)) - number(&quarter) - fee).to_string();
    let tip = chain.tip();
    until("the resumed index through the tip", || async {
        let index = app.api.index_of(&wallet, "native").await;
        if index["indexed_through"] == tip {
            return Ok(());
        }
        Err(index.to_string())
    })
    .await;
    reaches(app, &wallet, "native", "balance_confirmed", &left).await;
    let uri = format!("/v1/wallets/{}/indexes/native/legs", wallet.id);
    let all = app.api.get(&uri).await["legs"].clone();
    let all = all.as_array().cloned().unwrap_or_default();
    let resumed = &all[0];
    assert_eq!(
        (resumed["kind"].as_i64(), text(resumed, "amount")),
        (Some(6), "0".into()),
        "{all:?}"
    );
    let spends = all.iter().filter(|leg| leg["kind"] == 1).count();
    assert_eq!(spends, 1, "{all:?}");
}

/// Sets `wallet`'s native index to `state`.
async fn set_state(app: &App, wallet: &Wallet, state: &str) {
    let uri = format!("/v1/wallets/{}/indexes/native", wallet.id);
    let body = json!({ "index_state": state });
    let (status, answer) = app.api.call(Method::PATCH, &uri, Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
}

/// The network's kept transaction `txid`, once mined.
async fn transaction<T: TestChain>(app: &App, chain: &T, txid: &Value) -> Value {
    let network = chain.network().network;
    let uri = format!(
        "/v1/networks/{}/{}/transactions",
        network.chain.as_str(),
        network.name
    );
    until("the transaction mined", || async {
        let page = app.api.get(&uri).await;
        let all = page["transactions"].as_array().cloned().unwrap_or_default();
        match all.into_iter().find(|kept| kept["txid"] == *txid) {
            Some(kept) if kept["block"].is_string() => Ok(kept),
            other => Err(format!("{other:?}")),
        }
    })
    .await
}

/// Sends one unit of `asset` from `wallet` to `to`, expecting 422: its
/// message.
async fn refused(app: &App, wallet: &Wallet, asset: &str, to: &str) -> String {
    let body = json!({"asset": asset, "destination": to, "amount": "1"});
    let uri = format!("/v1/wallets/{}/transactions", wallet.id);
    let (status, answer) = app.api.call(Method::POST, &uri, Some(body)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{answer}");
    answer["message"].as_str().unwrap_or_default().to_owned()
}
