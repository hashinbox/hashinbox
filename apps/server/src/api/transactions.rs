//! A wallet's transactions: its history (`GET /v1/wallets/{id}/transactions`)
//! and an ordered send from it (`POST /v1/wallets/{id}/transactions`, a
//! queued job).

use std::sync::Arc;

use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
};
use base::{Decimal, Leg, TransactionState};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody, asset_info, wallet_address};
use crate::{
    api::AppState,
    models::Asset,
    usecase::workers::broadcast::{Payment, SendIntent},
};

/// Maximum transfers one send request accepts.
const MAX_TRANSFERS: usize = 50;

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(history, send))
}

/// Query of `GET /v1/wallets/{id}/transactions`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    /// An asset id of the wallet's network; default `native`.
    #[serde(default = "native")]
    #[param(default = "native")]
    asset: String,
    /// `next_cursor` of the previous page; absent for the first page.
    cursor: Option<String>,
    /// Page size, 1 to 1000.
    #[serde(default = "HistoryQuery::default_limit")]
    #[param(minimum = 1, maximum = 1000, default = 100)]
    limit: usize,
}

impl HistoryQuery {
    const fn default_limit() -> usize {
        100
    }
}

fn native() -> String {
    "native".to_owned()
}

/// The block a transaction was mined in.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct Block {
    /// Native position (Bitcoin/Ethereum height, Solana slot).
    position: u64,
    /// Produced-block count.
    height: u64,
    /// Block hash, hexadecimal, in the chain's native byte order.
    hash: String,
    /// Unix seconds, when the chain reports one; always present.
    #[schema(required)]
    timestamp: Option<u64>,
}

/// How a mined transaction executed.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    /// Executed.
    Succeeded,
    /// Mined but failed; only its fee moved.
    Reverted,
}

/// What a movement did to the wallet.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum MovementKind {
    /// Value arrived.
    Received,
    /// Value left.
    Spent,
    /// Value destroyed.
    Burned,
    /// Value created.
    Minted,
    /// The fee it paid.
    Fee,
}

/// One movement of the wallet's asset inside a transaction.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct Movement {
    /// Its order among the movements shown.
    index: u32,
    /// What it did.
    kind: MovementKind,
    /// Exact decimal in display units.
    amount: String,
}

/// One transaction of a wallet's history.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct Transaction {
    /// Chain-native transaction ID.
    transaction_id: String,
    /// The block it was mined in.
    block: Block,
    /// How it executed.
    outcome: Outcome,
    /// Why it reverted, as the chain reported it; always present.
    #[schema(required)]
    reason: Option<String>,
    /// The wallet's movements of the asset.
    movements: Vec<Movement>,
}

/// One page of `GET /v1/wallets/{id}/transactions`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = TransactionPage)]
struct HistoryResponse {
    /// The asset.
    asset: Asset,
    /// At most `limit` transactions in kept blocks, in chain order. Kept
    /// blocks reach the produced tip, so the last ones may still be taken
    /// back by a reorg.
    transactions: Vec<Transaction>,
    /// Opaque cursor of the next page; null on a short page. Always present.
    #[schema(required)]
    next_cursor: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/wallets/{id}/transactions",
    operation_id = "read_wallet_transactions",
    summary = "Read one page of a wallet's history",
    description = "The wallet's transactions in `asset`, in chain order, with only its own \
        movements of that asset; a fee is shown where this address paid it in the native \
        asset. Block rewards count in the balance but are not transactions.",
    params(WalletPath, HistoryQuery),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "One history page", body = HistoryResponse),
        (status = 400, description = "Malformed wallet ID, query, cursor, or limit",
            body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Network or asset is not configured", body = ErrorBody),
        (status = 503, description = "The store is unavailable", body = ErrorBody)
    ),
    tag = "transactions"
)]
async fn history(
    State(state): State<Arc<AppState>>,
    Path(path): Path<WalletPath>,
    query: Result<Query<HistoryQuery>, QueryRejection>,
) -> Result<Json<HistoryResponse>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    if query.limit == 0 || query.limit > 1_000 {
        return Err(Error::invalid_request(
            "history limit must be between 1 and 1000",
        ));
    }
    // A cursor is `<position>:<order>:<txid>` of the last transaction shown.
    let after = match query.cursor.as_deref() {
        None => None,
        Some(cursor) => {
            let mut parts = cursor.splitn(3, ':');
            let key = (
                parts.next().and_then(|part| part.parse::<u64>().ok()),
                parts.next().and_then(|part| part.parse::<u64>().ok()),
                parts.next(),
            );
            let (Some(position), Some(order), Some(txid)) = key else {
                return Err(Error::invalid_request("history cursor is invalid"));
            };
            Some((position, order, txid.to_owned()))
        }
    };
    let address = wallet_address(&state.payments, &path.id)?;
    let info = asset_info(&state.payments, &address.network, &query.asset)?;
    let page =
        crate::usecase::history::history(&state.db, &address, &info.asset, after, query.limit)
            .await?;
    let next_cursor = (page.len() == query.limit)
        .then(|| page.last())
        .flatten()
        .map(|(header, transaction)| {
            format!(
                "{}:{}:{}",
                header.position,
                transaction.order.unwrap_or(0),
                transaction.id.id
            )
        });
    let mut transactions = Vec::with_capacity(page.len());
    for (header, transaction) in page {
        let (outcome, reason) = match transaction.state {
            TransactionState::Reverted(reason) => (
                Outcome::Reverted,
                reason.map(|reason| format!("{reason:?}")),
            ),
            TransactionState::Pending | TransactionState::Succeeded => (Outcome::Succeeded, None),
        };
        let mut movements = Vec::with_capacity(transaction.legs.len());
        for (index, leg) in (0_u32..).zip(transaction.legs) {
            let kind = match leg {
                Leg::Received { .. } => MovementKind::Received,
                Leg::Spent { .. } => MovementKind::Spent,
                Leg::Burned { .. } => MovementKind::Burned,
                Leg::Minted { .. } => MovementKind::Minted,
                Leg::Fee { .. } => MovementKind::Fee,
            };
            let amount = info.display(leg.amount()).map_err(|error| {
                Error::unavailable(format!("indexed amount is not whole atomic units: {error}"))
            })?;
            movements.push(Movement {
                index,
                kind,
                amount: amount.to_string(),
            });
        }
        transactions.push(Transaction {
            transaction_id: transaction.id.id,
            block: Block {
                position: header.position,
                height: header.height,
                hash: hex::encode(&header.hash.0),
                timestamp: header.timestamp,
            },
            outcome,
            reason,
            movements,
        });
    }
    Ok(Json(HistoryResponse {
        asset: Asset::from(&info),
        transactions,
        next_cursor,
    }))
}

/// One ordered transfer of a send.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct TransferItem {
    /// Recipient address text, as the wallet's chain writes it.
    destination: String,
    /// Exact positive decimal in display units.
    amount: String,
}

/// `POST /v1/wallets/{id}/transactions` body: an ordered send from the
/// path's wallet in one asset.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = Send)]
#[serde(deny_unknown_fields)]
struct SendRequest {
    /// An asset id of the wallet's network; default `native`.
    #[serde(default = "native")]
    asset: String,
    /// The transfers, in execution order.
    #[schema(min_items = 1, max_items = 50)]
    transfers: Vec<TransferItem>,
}

/// An enqueued send.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = SendAccepted)]
struct SendResponse {
    /// The send job; `GET /v1/jobs/{id}` reports it.
    job: String,
}

#[utoipa::path(
    post,
    path = "/v1/wallets/{id}/transactions",
    operation_id = "send_transfers",
    summary = "Enqueue an ordered send from a wallet",
    description = "Enqueues one send job of `asset` from the keyed wallet: its transfers are \
        signed up front and broadcast in order. A 202 response means queued, not submitted: \
        `GET /v1/jobs/{id}` reports the outcome. Indexing records the transactions once they \
        are mined, for the assets the wallet is watched in. A watch-only wallet answers 422.",
    params(WalletPath),
    request_body = SendRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 202, description = "Send queued", body = SendResponse),
        (status = 400, description = "Malformed body, wallet ID, amount, or destination",
            body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Network or asset is not configured", body = ErrorBody),
        (status = 422, description = "A watch-only or unknown wallet", body = ErrorBody),
        (status = 503, description = "The queue or store is unavailable", body = ErrorBody)
    ),
    tag = "transactions"
)]
async fn send(
    State(state): State<Arc<AppState>>,
    Path(path): Path<WalletPath>,
    request: Result<Json<SendRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<SendResponse>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    if request.transfers.is_empty() || request.transfers.len() > MAX_TRANSFERS {
        return Err(Error::invalid_request(format!(
            "between 1 and {MAX_TRANSFERS} transfers are allowed"
        )));
    }
    let address = wallet_address(&state.payments, &path.id)?;
    let info = asset_info(&state.payments, &address.network, &request.asset)?;
    if !state
        .db
        .read(|connection| Ok(crate::models::wallet::db::find_keyed(connection)?.addresses()))
        .await?
        .contains(&address)
    {
        return Err(Error::unprocessable("the wallet holds no key here"));
    }
    let mut transfers = Vec::with_capacity(request.transfers.len());
    for (index, item) in request.transfers.into_iter().enumerate() {
        let at = |error: Error| error.at(index);
        let units = item
            .amount
            .parse::<Decimal>()
            .ok()
            .filter(|amount| amount > &Decimal::zero())
            .and_then(|amount| info.atomic(&amount).ok())
            .ok_or_else(|| {
                at(Error::invalid_request(
                    "amount must be a positive exact decimal within the asset's decimals",
                ))
            })?;
        let to = state
            .payments
            .parse(&address.network, &item.destination)
            .map_err(|error| at(error.into()))?;
        transfers.push(Payment {
            to: to.value,
            amount: units.to_string(),
        });
    }
    let intent = SendIntent {
        chain: address.network.chain.to_string(),
        network: address.network.name.clone(),
        address: address.value.clone(),
        asset: info.asset.id,
        transfers,
    };
    let job = intent
        .job()
        .map_err(|error| Error::invalid_request(error.to_string()))?;
    let id = job.id.to_string();
    tasks::Store::enqueue(&state.queue, job)
        .await
        .map_err(|error| Error::unavailable(error.to_string()))?;
    Ok((StatusCode::ACCEPTED, Json(SendResponse { job: id })))
}

/// The wallet a transaction route reads or sends from.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Path)]
struct WalletPath {
    /// `<chain>:<network>:<address>`.
    id: String,
}
