//! `/v1/wallets` and `/v1/wallets/{id}`: generate or watch, list, read and
//! archive wallets.

use std::sync::Arc;

use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
};
use indexing::{Birthday, BlockPosition, Holder as _, Watching as _};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};
use wallets::SecretBytes;

use super::{Error, ErrorBody};
use crate::{
    asset::AssetTag,
    models::{Label, Wallet},
    state::AppState,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(create, list))
        .routes(routes!(read, archive))
}

/// `POST /v1/wallets` body: generate a wallet for one configured asset, or
/// watch an existing address of it without its key.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = CreateWallet)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    /// The configured asset tag the wallet holds (`GET /v1/assets`).
    #[schema(value_type = String, pattern = "^[a-z0-9][a-z0-9-]{0,31}$")]
    asset: AssetTag,
    /// Address text in any form the asset's chain accepts. Present: watch this
    /// address without its key. Absent: generate a new wallet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    address: Option<String>,
    /// The operator's name for the wallet, 1 to 64 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false, value_type = Option<String>, min_length = 1, max_length = 64)]
    label: Option<Label>,
    /// Where observation begins: `"fresh"` for an address never used before,
    /// or the native position to inspect from. Absent means `"fresh"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    start: Option<Start>,
}

/// A wallet's birthday on the wire: `"fresh"` or a native position.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(untagged)]
enum Start {
    /// The address has never been used: observe from the next block.
    Fresh(Fresh),
    /// Observe from this native position.
    At(u64),
}

/// The `"fresh"` literal of a `Start`.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
enum Fresh {
    /// The address has never been used.
    Fresh,
}

#[utoipa::path(
    post,
    path = "/v1/wallets",
    operation_id = "create_wallet",
    summary = "Generate a wallet, or watch an address, for one configured asset",
    description = "Without `address`, generates a wallet, stores it with its key, then \
        watches it from creation. Keys never leave the server. Retrying creates another \
        wallet. With `address`, stores and observes that address from registration without \
        its key: the wallet is watch-only, and sending from it answers 422. `label` names \
        either. `start` is `\"fresh\"` (the default) or the native position observation begins \
        at; a position older than the network's backfill policy allows answers 400. An \
        archived wallet's address is refused: archiving is final.",
    request_body = CreateRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 201, description = "Wallet generated or address watched, and registered \
            for indexing", body = Wallet),
        (status = 400, description = "Malformed body, asset tag, label, address text, or a \
            start the backfill policy refuses", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Asset is not configured", body = ErrorBody),
        (status = 409, description = "The address is already an active or archived wallet, \
            an unobserved row of it holds another key, or the asset's network is being \
            rescanned",
            body = ErrorBody),
        (status = 503, description = "Wallet generation or registration is unavailable",
            body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn create(
    State(state): State<Arc<AppState>>,
    request: Result<Json<CreateRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Wallet>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    // No wallet is created on a network while it is being rescanned.
    let _admitted = state
        .wallets
        .admit(&request.asset)
        .map_err(Error::by_asset)?;
    let asset = state
        .wallets
        .asset(&request.asset)
        .map_err(Error::by_asset)?
        .id;
    // A watched address is the asset's own parse of the text; a generated
    // wallet is a new key of the asset and the wallet it builds.
    let (address, secret, generated) = match &request.address {
        Some(text) => {
            let address = state
                .wallets
                .parse(&request.asset, text)
                .map_err(Error::by_asset)?;
            (address, None, None)
        }
        None => {
            let material = state
                .wallets
                .secret(&request.asset)
                .await
                .map_err(Error::by_asset)?;
            let signer = state
                .wallets
                .from(&request.asset, SecretBytes::new(material.as_bytes()))
                .await
                .map_err(Error::by_asset)?;
            (signer.address().clone(), Some(material), Some(signer))
        }
    };
    // Archiving is final: a wallet that was observed and archived is never
    // observed again, so the store is asked before it would answer that
    // wallet back. A row never observed is an interrupted creation's, and is
    // taken over below.
    let archived = state.registry.wallets().await?.into_iter().any(|wallet| {
        wallet.archived_at().is_some()
            && wallet.birthday().is_some()
            && wallet.address() == &address
            && wallet.asset() == &asset
    });
    if archived {
        return Err(Error::conflict("the address belongs to an archived wallet"));
    }
    // Filed before anything observes it, so the key is durable first: an
    // interrupted creation leaves an archived, never-observed row and no
    // watched address without its key.
    let filed = state
        .registry
        .register(indexing::Registration {
            id: uuid::Uuid::now_v7().to_string(),
            address,
            asset,
            label: request.label.map(String::from),
            secret,
        })
        .await?;
    // The same rule, for a wallet archived since the question above.
    if filed.archived_at().is_some() && filed.birthday().is_some() {
        return Err(Error::conflict("the address belongs to an archived wallet"));
    }
    // The index's admission and backfill policy run before the row is
    // observed; a refusal leaves it archived.
    let birthday = match request.start {
        None | Some(Start::Fresh(Fresh::Fresh)) => Birthday::Fresh,
        Some(Start::At(position)) => Birthday::At(BlockPosition(position)),
    };
    state.index.watch(filed.id(), &filed, birthday).await?;
    // The signer: the one just generated, or an interrupted creation's key.
    let signer = match (generated, filed.secret()) {
        (Some(signer), _) => Some(signer),
        (None, Some(key)) => Some(
            state
                .wallets
                .from(&request.asset, SecretBytes::new(key.as_bytes()))
                .await
                .map_err(Error::by_asset)?,
        ),
        (None, None) => None,
    };
    if let Some(signer) = signer {
        state.signers.insert(filed.id().to_owned(), signer)?;
    }
    let wallet = Wallet::find(state.registry.as_ref(), &state.wallets, filed.id()).await?;
    Ok((StatusCode::CREATED, Json(wallet)))
}

/// Query of `GET /v1/wallets`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    /// Include archived wallets; they are left out by default.
    #[serde(default)]
    #[param(default = false)]
    archived: bool,
}

/// `GET /v1/wallets`: the wallets.
#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = WalletList)]
struct ListResponse {
    /// Wallets in ID order.
    wallets: Vec<Wallet>,
}

#[utoipa::path(
    get,
    path = "/v1/wallets",
    operation_id = "list_wallets",
    summary = "List the wallets",
    description = "Every active wallet and watched address in wallet ID order; with \
        `archived=true`, the archived ones too.",
    params(ListQuery),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "Wallets in ID order", body = ListResponse),
        (status = 400, description = "Malformed query", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 503, description = "The wallet collection is unavailable", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn list(
    State(state): State<Arc<AppState>>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<ListResponse>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    let wallets = Wallet::all(state.registry.as_ref(), &state.wallets)
        .await?
        .into_iter()
        .filter(|wallet| query.archived || !wallet.archived())
        .collect();
    Ok(Json(ListResponse { wallets }))
}

/// The wallet a route reads or archives.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Path)]
struct WalletPath {
    /// The wallet ID a create or watch call returned.
    id: String,
}

#[utoipa::path(
    get,
    path = "/v1/wallets/{id}",
    operation_id = "read_wallet",
    summary = "Read one wallet's public metadata",
    description = "The wallet's asset, network, canonical address, watch-only flag, label and \
        archive time, archived or not; never its key.",
    params(WalletPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The wallet's public metadata", body = Wallet),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Wallet is unknown", body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn read(
    State(state): State<Arc<AppState>>,
    Path(path): Path<WalletPath>,
) -> Result<Json<Wallet>, Error> {
    Ok(Json(
        Wallet::find(state.registry.as_ref(), &state.wallets, &path.id).await?,
    ))
}

#[utoipa::path(
    delete,
    path = "/v1/wallets/{id}",
    operation_id = "archive_wallet",
    summary = "Archive a wallet",
    description = "Marks the wallet archived and stops observing its address. Its history and \
        balance stay readable from what was indexed; sending from it answers 409. Archiving \
        again answers the same wallet. Archiving is final: its address cannot be created \
        again.",
    params(WalletPath),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "The archived wallet", body = Wallet),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Wallet is unknown", body = ErrorBody),
        (status = 409, description = "The wallet's network is being rescanned", body = ErrorBody),
        (status = 503, description = "The wallet could not be archived or unwatched",
            body = ErrorBody)
    ),
    tag = "wallets"
)]
async fn archive(
    State(state): State<Arc<AppState>>,
    Path(path): Path<WalletPath>,
) -> Result<Json<Wallet>, Error> {
    let found = Wallet::find(state.registry.as_ref(), &state.wallets, &path.id).await?;
    // A watch is not removed on a network while it is being rescanned.
    let _admitted = match found.asset().map(|tag| state.wallets.admit(tag)) {
        Ok(Ok(admitted)) => Some(admitted),
        Ok(Err(error)) if error.kind != wallets::WalletErrorKind::Unsupported => {
            return Err(error.into());
        }
        _ => None,
    };
    // The index archives the row under its publication permit, so no pass
    // commits for the wallet afterwards. A wallet of a network this process
    // does not serve has no index to unwatch it: the store archives it.
    state.index.unwatch(&path.id).await?;
    let stored = state.registry.wallet(&path.id).await?;
    if let Some(watch) = stored.and_then(|wallet| wallet.watch()) {
        let network = watch.address.network.clone();
        state.registry.forget(&network, &watch).await?;
    }
    state.signers.remove(&path.id)?;
    Ok(Json(
        Wallet::find(state.registry.as_ref(), &state.wallets, &path.id).await?,
    ))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;
    use wallets::{WalletErrorKind, testing::Factory};

    use indexing::Watching as _;

    use crate::{api::harness::App, models::Signers};

    #[tokio::test]
    async fn a_created_wallet_carries_its_asset_network_and_address() {
        let app = App::ready().await;
        let unauthorized = app
            .call("POST", "/v1/wallets", Some(json!({"asset": "btc"})), false)
            .await;
        assert_eq!(unauthorized.status, StatusCode::UNAUTHORIZED);

        let wallet = app.create("btc").await;
        assert_eq!(wallet["asset"], "btc");
        assert_eq!(
            wallet["network"],
            json!({"chain": "bitcoin", "name": "regtest"})
        );
        assert_eq!(wallet["address"], "addr-07");
        assert_eq!(wallet["watch_only"], false);

        let usdc = app.create("usdc").await;
        assert_eq!(usdc["asset"], "usdc");
        assert_eq!(
            usdc["network"],
            json!({"chain": "ethereum", "name": "mainnet"})
        );

        let sol = app.create("sol").await;
        assert_eq!(sol["asset"], "sol");
        assert_eq!(
            sol["network"],
            json!({"chain": "solana", "name": "localnet"})
        );
        assert!(app.sent().is_empty());
    }

    #[tokio::test]
    async fn unconfigured_wallet_asset_is_not_found() {
        let app = App::without_usdc(Factory::new(App::bitcoin())).await;
        let response = app
            .call("POST", "/v1/wallets", Some(json!({"asset": "usdc"})), true)
            .await;

        assert_eq!(response.status, StatusCode::NOT_FOUND);
        assert_eq!(response.json()["message"], "wallet asset is not configured");
    }

    #[tokio::test]
    async fn wallet_generation_errors_preserve_status_and_message() {
        for (kind, status, message) in [
            (
                WalletErrorKind::Unsupported,
                StatusCode::NOT_FOUND,
                "wallet asset is not configured",
            ),
            (
                WalletErrorKind::Generation,
                StatusCode::SERVICE_UNAVAILABLE,
                "the wallet double failed to generate",
            ),
            (
                WalletErrorKind::Conflict,
                StatusCode::CONFLICT,
                "the wallet double failed to generate",
            ),
            (
                WalletErrorKind::InvalidAddress,
                StatusCode::BAD_REQUEST,
                "the wallet double failed to generate",
            ),
            (
                WalletErrorKind::NotFound,
                StatusCode::NOT_FOUND,
                "the wallet double failed to generate",
            ),
        ] {
            let app = App::without_usdc(Factory::new(App::bitcoin()).failing(kind)).await;
            let response = app
                .call("POST", "/v1/wallets", Some(json!({"asset": "btc"})), true)
                .await;

            assert_eq!(response.status, status, "{kind:?}");
            assert_eq!(response.json(), json!({"message": message}), "{kind:?}");
            assert!(app.sent().is_empty());
        }
    }

    #[tokio::test]
    async fn invalid_wallet_asset_uses_the_json_error_contract() {
        let app = App::ready().await;
        for (asset, status, message) in [
            (
                "USD",
                StatusCode::BAD_REQUEST,
                "request body must match the documented JSON schema",
            ),
            (
                "usd",
                StatusCode::NOT_FOUND,
                "wallet asset is not configured",
            ),
        ] {
            let response = app
                .call("POST", "/v1/wallets", Some(json!({"asset": asset})), true)
                .await;
            assert_eq!(response.status, status, "{asset}");
            assert_eq!(response.json()["message"], message, "{asset}");
        }
    }

    /// A watch the index definitely refuses leaves no wallet behind.
    #[tokio::test]
    async fn a_refused_watch_leaves_no_wallet() {
        let app = App::ready().await;
        let first = app.create("btc").await;
        let response = app
            .call("POST", "/v1/wallets", Some(json!({"asset": "btc"})), true)
            .await;
        assert_eq!(
            response.status,
            StatusCode::CONFLICT,
            "the double's one address"
        );
        assert_eq!(app.ids().await, [first["id"].as_str().expect("an id")]);
        let listed = app.call("GET", "/v1/wallets", None, true).await;
        assert_eq!(listed.json()["wallets"].as_array().map(Vec::len), Some(1));
    }

    /// A label is kept and published; an unusable one is refused before
    /// anything is created.
    #[tokio::test]
    async fn a_wallet_carries_its_label() {
        let app = App::ready().await;
        let body = json!({"asset": "btc", "label": "treasury"});
        let created = app.call("POST", "/v1/wallets", Some(body), true).await;
        assert_eq!(created.status, StatusCode::CREATED);
        assert_eq!(created.json()["label"], "treasury");
        assert_eq!(created.json()["archived_at"], serde_json::Value::Null);
        let body = json!({"asset": "sol", "address": "ADDR-Seen", "label": "seen"});
        let watched = app.call("POST", "/v1/wallets", Some(body), true).await;
        assert_eq!(watched.json()["label"], "seen");
        assert_eq!(watched.json()["watch_only"], true);
        for label in [json!(""), json!("a\nb"), json!("x".repeat(65)), json!(7)] {
            let body = json!({"asset": "usdc", "label": label});
            let refused = app.call("POST", "/v1/wallets", Some(body), true).await;
            assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{label}");
        }
        assert_eq!(app.ids().await.len(), 2);
    }

    /// Archiving unwatches the wallet, keeps its history and balance
    /// readable, refuses its sends, answers again the same, and hides it from
    /// the default list.
    #[tokio::test]
    async fn an_archived_wallet_is_read_not_sent_from_and_listed_on_request() {
        let app = App::ready().await;
        let id = app.wallet_id().await;
        let other = app.create("sol").await;
        app.mine().await;
        let path = format!("/v1/wallets/{id}");
        let archived = app.call("DELETE", &path, None, true).await;
        assert_eq!(archived.status, StatusCode::OK);
        let at = archived.json()["archived_at"].as_u64().expect("archived");
        let again = app.call("DELETE", &path, None, true).await;
        assert_eq!(again.json()["archived_at"].as_u64(), Some(at), "idempotent");
        let watched = app
            .state
            .index
            .watched(&App::bitcoin())
            .await
            .expect("watches");
        assert!(watched.iter().all(|watch| watch.id != id), "unwatched");

        assert_eq!(
            app.call("GET", &path, None, true).await.json(),
            archived.json()
        );
        let history = app
            .call("GET", &format!("{path}/transactions"), None, true)
            .await;
        assert_eq!(history.status, StatusCode::OK);
        assert_eq!(
            history.json()["transactions"][0]["movements"][0]["amount"],
            "1.5"
        );
        let balance = app
            .call("GET", &format!("{path}/balance"), None, true)
            .await;
        assert_eq!(balance.json()["amount"], "1.5");
        let send = json!({"destination": "addr-destination", "amount": "1"});
        let refused = app
            .call("POST", &format!("{path}/transactions"), Some(send), true)
            .await;
        assert_eq!(refused.status, StatusCode::CONFLICT);
        assert!(app.sent().is_empty());

        let listed = app.call("GET", "/v1/wallets", None, true).await.json();
        assert_eq!(listed["wallets"].as_array().map(Vec::len), Some(1));
        assert_eq!(listed["wallets"][0]["id"], other["id"]);
        let all = app
            .call("GET", "/v1/wallets?archived=true", None, true)
            .await
            .json();
        assert_eq!(all["wallets"].as_array().map(Vec::len), Some(2));
        let missing = app.call("DELETE", "/v1/wallets/missing", None, true).await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        let malformed = app
            .call("GET", "/v1/wallets?archived=maybe", None, true)
            .await;
        assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
    }

    /// Startup rebuilds the key of every observed, keyed wallet and watches
    /// nothing itself: an archived wallet's key is not loaded, a watch-only
    /// wallet has none, and the index's watch list is exactly the store's.
    #[tokio::test]
    async fn a_start_restores_keys_from_the_rows_and_watches_nothing() {
        let app = App::ready().await;
        let kept = app.wallet_id().await;
        let archived = app.create("sol").await["id"]
            .as_str()
            .expect("an id")
            .to_owned();
        let body = json!({"asset": "usdc", "address": "ADDR-Seen"});
        let seen = app.call("POST", "/v1/wallets", Some(body), true).await;
        assert_eq!(seen.status, StatusCode::CREATED);
        let path = format!("/v1/wallets/{archived}");
        assert_eq!(
            app.call("DELETE", &path, None, true).await.status,
            StatusCode::OK
        );
        let state = &app.state;
        let mut watched = Vec::new();
        for network in [App::bitcoin(), App::solana(), App::ethereum()] {
            watched.push(state.index.watched(&network).await.expect("watches"));
        }

        let signers = Signers::load(
            state.registry.as_ref(),
            &state.wallets,
            state.index.networks(),
        )
        .await
        .expect("loaded");
        assert_eq!(signers.ids(), std::slice::from_ref(&kept));
        for (network, before) in [App::bitcoin(), App::solana(), App::ethereum()]
            .iter()
            .zip(watched)
        {
            let after = state.index.watched(network).await.expect("watches");
            assert_eq!(after, before, "nothing re-registered");
        }
        let served: Vec<_> = state
            .index
            .networks()
            .iter()
            .filter(|network| **network != App::bitcoin())
            .cloned()
            .collect();
        let unserved = Signers::load(state.registry.as_ref(), &state.wallets, &served)
            .await
            .expect("loaded");
        assert!(unserved.ids().is_empty(), "an unserved network's key waits");
    }

    /// Archiving is final: creating a wallet of an archived address, keyed
    /// or watch-only, is refused and files nothing.
    #[tokio::test]
    async fn an_archived_address_is_refused_not_restored() {
        let app = App::ready().await;
        let body = json!({"asset": "btc", "address": "ADDR-Back"});
        let first = app
            .call("POST", "/v1/wallets", Some(body), true)
            .await
            .json();
        let id = first["id"].as_str().expect("an id").to_owned();
        let keyed = app.wallet_id().await;
        for archived in [&id, &keyed] {
            let path = format!("/v1/wallets/{archived}");
            assert_eq!(
                app.call("DELETE", &path, None, true).await.status,
                StatusCode::OK
            );
        }
        assert!(app.state.signers.get(&keyed).expect("signers").is_none());
        for body in [
            json!({"asset": "btc", "address": "addr-back"}),
            json!({"asset": "btc", "address": "addr-07"}),
            json!({"asset": "btc"}),
        ] {
            let again = app.call("POST", "/v1/wallets", Some(body), true).await;
            assert_eq!(again.status, StatusCode::CONFLICT);
        }
        let mut ids = vec![id, keyed];
        ids.sort();
        assert_eq!(app.ids().await, ids, "no wallet filed");
        let watched = app
            .state
            .index
            .watched(&App::bitcoin())
            .await
            .expect("watches");
        assert!(watched.is_empty(), "nothing observed again");
    }

    /// `start` is `"fresh"` or a native position; a position the backfill
    /// policy refuses answers 400 and observes nothing.
    #[tokio::test]
    async fn a_wallet_starts_fresh_or_at_a_position_the_policy_admits() {
        let app = App::ready().await;
        // A checkpoint at 8, so the policy admits starts from 5.
        app.mine().await;
        let start = |app: &App, id: String| {
            let index = app.state.index.clone();
            async move {
                index
                    .watched(&App::bitcoin())
                    .await
                    .expect("watches")
                    .into_iter()
                    .find(|watch| watch.id == id)
                    .map(|watch| watch.start.0)
            }
        };
        for (body, expected) in [
            (
                json!({"asset": "btc", "address": "addr-fresh", "start": "fresh"}),
                9,
            ),
            (json!({"asset": "btc", "address": "addr-at", "start": 6}), 6),
        ] {
            let created = app.call("POST", "/v1/wallets", Some(body), true).await;
            assert_eq!(created.status, StatusCode::CREATED);
            let id = created.json()["id"].as_str().expect("an id").to_owned();
            assert_eq!(start(&app, id).await, Some(expected));
        }
        let old = json!({"asset": "btc", "address": "addr-old", "start": 0});
        let refused = app.call("POST", "/v1/wallets", Some(old), true).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        let malformed = json!({"asset": "btc", "address": "addr-bad", "start": "later"});
        let malformed = app.call("POST", "/v1/wallets", Some(malformed), true).await;
        assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
        let listed = app.call("GET", "/v1/wallets", None, true).await.json();
        assert_eq!(listed["wallets"].as_array().map(Vec::len), Some(2));
    }

    #[tokio::test]
    async fn durable_key_never_appears_in_wallet_responses() {
        let app = App::ready().await;
        let created = app
            .call("POST", "/v1/wallets", Some(json!({"asset": "btc"})), true)
            .await;
        assert_eq!(created.status, StatusCode::CREATED);
        let id = created.json()["id"].as_str().unwrap().to_owned();
        assert_eq!(app.ids().await, std::slice::from_ref(&id));
        let listed = app.call("GET", "/v1/wallets", None, true).await;
        let fetched = app
            .call("GET", &format!("/v1/wallets/{id}"), None, true)
            .await;
        for response in [created, listed, fetched] {
            let text = String::from_utf8(response.body.to_vec()).unwrap();
            for private in ["secret", "material", "ciphertext", &hex::encode([7; 32])] {
                assert!(
                    !text.contains(private),
                    "wallet response reveals signing material"
                );
            }
        }
    }

    #[tokio::test]
    async fn watched_addresses_have_history_and_balance_but_no_sends() {
        let app = App::ready().await;
        let watched = app
            .call(
                "POST",
                "/v1/wallets",
                Some(json!({"asset": "btc", "address": "ADDR-Watched"})),
                true,
            )
            .await;
        assert_eq!(watched.status, StatusCode::CREATED);
        let wallet = watched.json();
        let id = wallet["id"].as_str().expect("watch ID").to_owned();
        assert_eq!(wallet["address"], "addr-watched");
        assert_eq!(wallet["watch_only"], true);
        assert_eq!(
            app.ids().await,
            std::slice::from_ref(&id),
            "the row is the wallet"
        );
        app.mine().await;

        let history = app
            .call("GET", &format!("/v1/wallets/{id}/transactions"), None, true)
            .await;
        assert_eq!(history.status, StatusCode::OK);
        let movement = &history.json()["transactions"][0]["movements"][0];
        assert_eq!(movement["to"]["value"], "addr-watched");
        assert_eq!(movement["amount"], "1.5");

        let balance = app
            .call("GET", &format!("/v1/wallets/{id}/balance"), None, true)
            .await;
        assert_eq!(
            balance.status,
            StatusCode::OK,
            "a fold over indexed movements needs no key"
        );
        assert_eq!(balance.json()["amount"], "1.5");
        for amount in ["1", "all"] {
            let send = app
                .call(
                    "POST",
                    &format!("/v1/wallets/{id}/transactions"),
                    Some(json!({"destination": "addr-destination", "amount": amount})),
                    true,
                )
                .await;
            assert_eq!(send.status, StatusCode::UNPROCESSABLE_ENTITY);
            let batch = app
                .call(
                    "POST",
                    "/v1/transactions",
                    Some(json!({"transfers": [
                        {"wallet_id": id, "destination": "addr-destination", "amount": amount}
                    ]})),
                    true,
                )
                .await;
            assert_eq!(batch.status, StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(batch.json()["failed_index"], 0);
        }
        assert!(app.sent().is_empty());
    }

    #[tokio::test]
    async fn a_watched_address_is_refused_as_its_asset_refuses_it() {
        let app = App::ready().await;
        for (body, status) in [
            (
                json!({"asset": "btc", "address": "not-an-address"}),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({"asset": "doge", "address": "addr-a"}),
                StatusCode::NOT_FOUND,
            ),
        ] {
            let response = app.call("POST", "/v1/wallets", Some(body), true).await;
            assert_eq!(response.status, status);
        }

        let first = json!({"asset": "btc", "address": "addr-twice"});
        let created = app
            .call("POST", "/v1/wallets", Some(first.clone()), true)
            .await;
        assert_eq!(created.status, StatusCode::CREATED);
        let again = app.call("POST", "/v1/wallets", Some(first), true).await;
        assert_eq!(
            again.status,
            StatusCode::CONFLICT,
            "an address already watched is refused, not watched twice"
        );
    }

    #[tokio::test]
    async fn a_wallet_reads_back_exactly_as_created() {
        let app = App::ready().await;
        let wallet = app.create("btc").await;
        let id = wallet["id"].as_str().expect("wallet ID");

        let read = app
            .call("GET", &format!("/v1/wallets/{id}"), None, true)
            .await;

        assert_eq!(read.status, StatusCode::OK);
        assert_eq!(read.json(), wallet);
    }

    #[tokio::test]
    async fn missing_wallet_read_preserves_the_not_found_error_contract() {
        let app = App::ready().await;
        let response = app
            .call("GET", "/v1/wallets/missing-wallet", None, true)
            .await;

        assert_eq!(response.status, StatusCode::NOT_FOUND);
        assert_eq!(response.json(), json!({"message": "wallet does not exist"}));
        assert!(app.sent().is_empty());
    }
}
