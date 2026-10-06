//! A wallet's transactions: its history (`GET /v1/wallets/{id}/transactions`),
//! one transfer from it (`POST /v1/wallets/{id}/transactions`), and an ordered
//! batch across wallets of one asset (`POST /v1/transactions`), with the
//! history page models and the opaque history cursor.

use std::sync::Arc;

use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody};
use crate::{
    models::{Asset, Block, Completeness, Network, Wallet},
    state::AppState,
};

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(history, send))
        .routes(routes!(send_batch))
}

/// Query of `GET /v1/wallets/{id}/transactions`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    /// `next_cursor` of the previous page; absent for the first page.
    cursor: Option<String>,
    /// Page size, 1 to 1000.
    #[serde(default = "HistoryQuery::default_limit")]
    #[param(minimum = 1, maximum = 1000, default = 100)]
    limit: usize,
}

impl TryFrom<HistoryQuery> for indexing::Page {
    type Error = Error;

    fn try_from(query: HistoryQuery) -> Result<Self, Self::Error> {
        if query.limit == 0 || query.limit > 1_000 {
            return Err(Error::invalid_request(
                "history limit must be between 1 and 1000",
            ));
        }
        let after = query.cursor.as_deref().map(Cursor::decode).transpose()?;
        Ok(Self {
            after,
            limit: query.limit,
        })
    }
}

impl HistoryQuery {
    const fn default_limit() -> usize {
        100
    }
}

#[utoipa::path(
    get,
    path = "/v1/wallets/{id}/transactions",
    operation_id = "read_wallet_transactions",
    summary = "Read one page of a wallet's history",
    description = "The wallet's canonical history in the wallet's own asset. `transactions` \
        is confirmed and append-only, so its cursor stays valid however far the checkpoint \
        moves; `pending` carries the transactions above the confirmed line and is populated \
        on the first page only; `completeness` says how much of the history behind the page the \
        index can prove it holds, in the same three words the balance uses.",
    params(WalletPath, HistoryQuery),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "One history page", body = HistoryResponse),
        (status = 400, description = "Malformed query, cursor, or limit", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Wallet is unknown", body = ErrorBody),
        (status = 500, description = "The next cursor could not be encoded", body = ErrorBody),
        (status = 503, description = "The index is unavailable", body = ErrorBody)
    ),
    tag = "transactions"
)]
async fn history(
    State(state): State<Arc<AppState>>,
    Path(path): Path<WalletPath>,
    query: Result<Query<HistoryQuery>, QueryRejection>,
) -> Result<Json<HistoryResponse>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    let page = query.try_into()?;
    let wallet = Wallet::find(state.registry.as_ref(), &state.wallets, &path.id).await?;
    let subject = wallet.subject()?;
    // The reader that owns the wallet's network answers in atomic units;
    // the page projects them by each asset's declared unit.
    let page = state.history.history(subject, page).await?;
    // The page is refused whole, before any of it is converted, when it left
    // the queried network (another network's identity would reach a caller as
    // if it were theirs) or is denominated in anything the query did not ask
    // for. A fee is the chain's own currency on every supported chain, which
    // is why `native` is taken from the queried network, not from the page.
    let network = &subject.address.network;
    let native = base::AssetId::native(network.chain.clone());
    if page
        .next
        .as_ref()
        .is_some_and(|next| !next.transaction.belongs_to(network))
    {
        return Err(Error::indexed(
            "indexed history cursor does not belong to the requested network",
        ));
    }
    for observed in page.transactions.iter().chain(&page.pending) {
        let transaction = &observed.transaction;
        let fee = transaction.fee.as_ref();
        if !transaction.id.belongs_to(network) {
            return Err(Error::indexed(
                "indexed transaction does not belong to the requested network",
            ));
        }
        let payer = fee.and_then(|fee| fee.payer.as_ref());
        if payer.is_some_and(|payer| !payer.belongs_to(network)) {
            return Err(Error::indexed(
                "indexed fee payer does not belong to the transaction network",
            ));
        }
        if transaction
            .movements
            .iter()
            .any(|movement| movement.asset != subject.asset)
        {
            return Err(Error::indexed(
                "indexed history contains an asset outside the query",
            ));
        }
        if fee.is_some_and(|fee| fee.asset != native) {
            return Err(Error::indexed(
                "indexed fee is not priced in the chain's own currency",
            ));
        }
    }
    // The index answers in exact atomic units and never learns an asset's
    // decimals: each amount is scaled here alone, by the unit its own asset's
    // chain declared.
    let scaled = |asset: &base::AssetId, amount: &base::Decimal| {
        let unit = state
            .units
            .of(network, asset)
            .ok_or_else(Error::undeclared)?;
        let display = unit.display(amount).map_err(Error::unscalable)?;
        Ok::<_, Error>((Asset::new(asset, unit), display.to_string()))
    };
    let movement = |movement: indexing::ValueMovement| {
        let (asset, amount) = scaled(&movement.asset, &movement.amount)?;
        Ok::<_, Error>(Movement {
            id: movement.id.0.clone(),
            kind: movement.kind().into(),
            asset,
            amount,
            from: movement.from().cloned().map(Into::into),
            to: movement.to().cloned().map(Into::into),
        })
    };
    let fee = |fee: indexing::NetworkFee| {
        let (asset, amount) = scaled(&fee.asset, &fee.amount)?;
        let payer = fee.payer.map(Into::into);
        Ok::<_, Error>(Fee {
            asset,
            amount,
            payer,
        })
    };
    let transaction = |observed: indexing::ObservedTransaction| {
        let transaction = observed.transaction;
        Ok::<_, Error>(Transaction {
            network: transaction.id.network.clone().into(),
            transaction_id: transaction.id.id.to_string(),
            status: observed.status.into(),
            movements: transaction
                .movements
                .into_iter()
                .map(&movement)
                .collect::<Result<_, Error>>()?,
            fee: transaction.fee.map(&fee).transpose()?,
        })
    };
    let convert = |observed: Vec<indexing::ObservedTransaction>| {
        observed
            .into_iter()
            .map(&transaction)
            .collect::<Result<Vec<_>, Error>>()
    };
    Ok(Json(HistoryResponse {
        next_cursor: page.next.as_ref().map(Cursor::encode).transpose()?,
        completeness: page.completeness.into(),
        checkpoint: page.checkpoint.map(Into::into),
        transactions: convert(page.transactions)?,
        pending: convert(page.pending)?,
    }))
}

/// Kind of a [`Movement`].
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum MovementKind {
    /// Value moved from one address to another (account chains).
    Transfer,
    /// A spent output (UTXO chains); `from` is its owner.
    Input,
    /// A created output (UTXO chains); `to` is its owner.
    Output,
    /// Value created at `to`.
    Mint,
    /// Value destroyed at `from`.
    Burn,
}

impl From<indexing::MovementKind> for MovementKind {
    fn from(value: indexing::MovementKind) -> Self {
        match value {
            indexing::MovementKind::Transfer => Self::Transfer,
            indexing::MovementKind::Input => Self::Input,
            indexing::MovementKind::Output => Self::Output,
            indexing::MovementKind::Mint => Self::Mint,
            indexing::MovementKind::Burn => Self::Burn,
        }
    }
}

/// One value change of the wallet's asset inside a transaction.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct Movement {
    /// Chain-derived movement ID.
    id: String,
    /// What kind of value change this is.
    kind: MovementKind,
    /// The moved asset.
    asset: Asset,
    /// Exact decimal in display units.
    amount: String,
    /// Sender or spent-output owner, when there is one; always present.
    #[schema(required)]
    from: Option<Address>,
    /// Recipient or created-output owner, when there is one; always present.
    #[schema(required)]
    to: Option<Address>,
}

/// A transaction fee, always in the chain's native asset.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct Fee {
    /// The chain's native asset.
    asset: Asset,
    /// Exact decimal in display units.
    amount: String,
    /// Fee payer, when the chain attributes one; always present.
    #[schema(required)]
    payer: Option<Address>,
}

/// Where a history transaction stands relative to its chain's confirmed line.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Status {
    /// In a canonical block above the confirmed line; it may still revert.
    Pending {
        /// The including block.
        block: Block,
        /// Blocks from inclusion to the newest indexed block, inclusive.
        confirmations: u64,
    },
    /// Committed at or below the confirmed line; it will not revert.
    Confirmed {
        /// The including block.
        block: Block,
        /// Blocks from inclusion to the newest indexed block, inclusive.
        confirmations: u64,
    },
    /// Included but failed on chain.
    Failed {
        /// The including block.
        block: Block,
        /// Chain-reported failure reason, when available; always present.
        #[schema(required)]
        reason: Option<String>,
    },
}

impl From<indexing::TransactionStatus> for Status {
    fn from(value: indexing::TransactionStatus) -> Self {
        match value {
            indexing::TransactionStatus::Pending {
                block,
                confirmations,
            } => Self::Pending {
                block: block.into(),
                confirmations,
            },
            indexing::TransactionStatus::Confirmed {
                block,
                confirmations,
            } => Self::Confirmed {
                block: block.into(),
                confirmations,
            },
            indexing::TransactionStatus::Failed { block, reason } => Self::Failed {
                block: block.into(),
                reason,
            },
        }
    }
}

/// One transaction of a wallet's history.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct Transaction {
    /// The transaction's chain and network.
    network: Network,
    /// Chain-native transaction ID.
    transaction_id: String,
    /// Inclusion, confirmation, or failure at a block.
    status: Status,
    /// Movements of the wallet's asset.
    movements: Vec<Movement>,
    /// The fee, when one is attributed; always present.
    #[schema(required)]
    fee: Option<Fee>,
}

/// One page of `GET /v1/wallets/{id}/transactions`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = TransactionPage)]
struct HistoryResponse {
    /// The confirmed line the page was read at; null before the first sync.
    /// Always present.
    #[schema(required)]
    checkpoint: Option<Block>,
    /// At most `limit` confirmed transactions; fewer only on the last page.
    /// Append-only: a transaction already returned on a page is never removed
    /// or reordered, so a `next_cursor` stays valid however long it is held.
    transactions: Vec<Transaction>,
    /// Transactions in canonical blocks above the confirmed line, oldest
    /// first. They are not in `transactions` because they can still revert,
    /// which would break that list's ordering guarantee. Populated on the
    /// first page only (`cursor` absent), and bounded by the chain's
    /// confirmation depth.
    pending: Vec<Transaction>,
    /// Opaque cursor of the next page; null when no transaction follows.
    /// Always present.
    #[schema(required)]
    next_cursor: Option<String>,
    /// How much of the history behind this page the index can prove it holds.
    /// `next_cursor` cannot carry it — that speaks about what follows the page,
    /// and a backfill inserts rows *before* it — so a caller paging to the end
    /// mid-backfill reads `incomplete` rather than a complete export.
    completeness: Completeness,
}

/// The opaque `cursor` of a history page: a position in one address's
/// confirmed history, and nothing else.
///
/// It used to carry the checkpoint it was issued at, so the index could refuse
/// a cursor whose checkpoint had moved. That refusal is gone with the journal:
/// the confirmed store is append-only, so a checkpoint that moved can only
/// have appended *above* this position, and an ascending keyset page is stable
/// however long a caller holds it.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    chain: String,
    network: String,
    transaction: String,
    height: u64,
    /// The transaction's native place in its block, which is the middle
    /// component of the page order. Without it a cursor could not name a
    /// position at all once two transactions of one block are stored.
    order: u64,
}

impl Cursor {
    fn encode(position: &indexing::HistoryPosition) -> Result<String, Error> {
        use base64::Engine;

        let bytes = serde_json::to_vec(&Self {
            chain: position.transaction.network.chain.to_string(),
            network: position.transaction.network.name.clone(),
            transaction: position.transaction.id.to_string(),
            height: position.height.0,
            order: position.transaction_order,
        })
        .map_err(|_| Self::invalid_response("history cursor could not be encoded"))?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }

    fn decode(value: &str) -> Result<indexing::HistoryPosition, Error> {
        use base64::Engine;

        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| Self::invalid_request("history cursor is invalid"))?;
        let cursor: Self = serde_json::from_slice(&bytes)
            .map_err(|_| Self::invalid_request("history cursor is invalid"))?;
        if cursor.chain.is_empty() || cursor.network.is_empty() || cursor.transaction.is_empty() {
            return Err(Self::invalid_request("history cursor is invalid"));
        }
        let network = base::Network {
            chain: base::ChainName::new(cursor.chain),
            name: cursor.network,
        };
        Ok(indexing::HistoryPosition {
            height: base::BlockHeight(cursor.height),
            transaction_order: cursor.order,
            transaction: base::TransactionRef {
                network,
                id: cursor.transaction.into(),
            },
        })
    }

    fn invalid_request(message: impl Into<String>) -> Error {
        Error::invalid_request(message)
    }

    fn invalid_response(message: impl Into<String>) -> Error {
        Error::invalid_response(message)
    }
}

/// `POST /v1/wallets/{id}/transactions` body: one transfer from the path's wallet.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = Transfer)]
#[serde(deny_unknown_fields)]
struct SendRequest {
    /// Recipient address text, as the wallet's chain writes it.
    destination: String,
    /// Exact decimal in display units, or `all` for the available balance after fees.
    amount: String,
}

/// A submitted, not yet confirmed, transaction.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = TransferAccepted)]
struct SendResponse {
    /// Chain-native transaction ID.
    transaction_id: String,
}

#[utoipa::path(
    post,
    path = "/v1/wallets/{id}/transactions",
    operation_id = "send_transfer",
    summary = "Submit one transfer from a wallet",
    description = "Submits one transfer through the wallet's configured asset. Every asset \
        uses this shared route; no chain-specific transaction route exists. A watch-only wallet \
        answers 422. A 202 response means submitted, not confirmed.",
    params(WalletPath),
    request_body = SendRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 202, description = "Transaction submitted", body = SendResponse),
        (status = 400, description = "Malformed body, amount, or destination", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Wallet is unknown", body = ErrorBody),
        (status = 409, description = "The wallet's network is being rescanned", body = ErrorBody),
        (status = 422, description = "Deterministic rejection, or a watch-only wallet",
            body = ErrorBody),
        (status = 503, description = "Transaction unavailable or exact-envelope submission \
            outcome ambiguous; ambiguous_transaction_id is present only for an unknown outcome",
            body = ErrorBody)
    ),
    tag = "transactions"
)]
async fn send(
    State(state): State<Arc<AppState>>,
    Path(path): Path<WalletPath>,
    _query: SubmitQuery,
    request: Result<Json<SendRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<SendResponse>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let wallet = Wallet::find(state.registry.as_ref(), &state.wallets, &path.id).await?;
    wallet.subject()?;
    // The parse only produces addresses of the wallet's own asset network,
    // which is why no separate same-network check exists.
    let amount = amount(&request.amount)?;
    let destination = destination(&state, &wallet, &request.destination)?;
    // Sending is the asset's capability, not the wallet's: an asset this
    // deployment only receives has no author to hand out, and that one
    // absence is the whole refusal. An archived wallet sends nothing (409).
    let signer = wallet.signer(&state.signers)?;
    let author = state.wallets.author(wallet.asset()?, &signer)?;
    // The asset's send queue refuses a send while its network is paused for a
    // rescan (409), and the rescan waits for the sends it admitted.
    let id = author.send(destination, amount).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(SendResponse {
            transaction_id: id.to_string(),
        }),
    ))
}

/// One ordered item of a `TransferBatch`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct BatchedTransfer {
    /// The sending wallet.
    wallet_id: String,
    /// Recipient address text, as the wallet's chain writes it.
    destination: String,
    /// Exact decimal in display units, or `all` to sweep spendable funds.
    amount: String,
}

/// `POST /v1/transactions` body: an ordered batch of one wallet asset.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = TransferBatch)]
#[serde(deny_unknown_fields)]
struct BatchRequest {
    /// Transfers of one configured asset, in execution order.
    #[schema(min_items = 1, max_items = 50)]
    transfers: Vec<BatchedTransfer>,
}

/// Result of an accepted batch.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = BatchAccepted)]
struct BatchResponse {
    /// Accepted transaction IDs in chain-native submission order.
    transaction_ids: Vec<String>,
}

#[utoipa::path(
    post,
    path = "/v1/transactions",
    operation_id = "send_transfer_batch",
    summary = "Submit an ordered batch of transfers",
    description = "Submits one ordered exact-asset batch of one configured wallet asset. \
        Every asset uses this shared route; no chain-specific transaction route exists. Requests \
        mixing wallet assets are rejected before any transaction is submitted. The asset's chain \
        may group occurrences into one transaction, or reserve consecutive sequence numbers per \
        sender and prepare the whole batch before submitting exact envelopes in request order. \
        Definitely acknowledged transaction IDs are returned only \
        when a non-empty prefix was accepted, and a failed index is returned only when one \
        original occurrence truthfully failed.",
    request_body = BatchRequest,
    security(("bearerAuth" = [])),
    responses(
        (status = 202, description = "Asset batch submitted", body = BatchResponse),
        (status = 400, description = "Malformed body, batch size, amount, or destination",
            body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "A batch wallet is unknown", body = ErrorBody),
        (status = 409, description = "The batch asset's network is being rescanned",
            body = ErrorBody),
        (status = 422, description = "Definite batch failure; transaction_ids is present only for \
            a definitely acknowledged prefix and failed_index only for a truthful item-scoped \
            failure",
            body = ErrorBody),
        (status = 503, description = "Batch unavailable or exact-envelope submission outcome \
            ambiguous; transaction_ids is present only for a definitely acknowledged prefix, \
            failed_index only for a truthful item-scoped failure, and ambiguous_transaction_id \
            only for an unknown outcome",
            body = ErrorBody)
    ),
    tag = "transactions"
)]
async fn send_batch(
    State(state): State<Arc<AppState>>,
    _query: SubmitQuery,
    request: Result<Json<BatchRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<BatchResponse>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    if request.transfers.len() > ::wallets::MAX_TRANSFERS {
        return Err(Error::invalid_request(format!(
            "at most {} transfers are allowed",
            ::wallets::MAX_TRANSFERS
        )));
    }
    // Every transfer resolved to the wallet that signs it and a destination
    // parsed by that wallet's own asset; a failure names the transfer's index.
    let mut tag = None;
    let mut transfers = Vec::with_capacity(request.transfers.len());
    for (failed_index, item) in request.transfers.into_iter().enumerate() {
        let at = |error: Error| error.at(failed_index);
        let amount = amount(&item.amount).map_err(at)?;
        let found = Wallet::find(state.registry.as_ref(), &state.wallets, &item.wallet_id)
            .await
            .map_err(|error| at(error.into()))?;
        let asset = found.asset().map_err(|error| at(error.into()))?.clone();
        let wallet = found
            .signer(&state.signers)
            .map_err(|error| at(error.into()))?;
        let to = destination(&state, &found, &item.destination).map_err(at)?;
        if tag.as_ref().is_some_and(|known| *known != asset) {
            return Err(Error::invalid_batch(
                failed_index,
                "all transfers must use the same wallet asset",
            ));
        }
        tag = Some(asset);
        transfers.push(::wallets::Transfer { wallet, to, amount });
    }
    let tag = tag.ok_or_else(|| Error::invalid_request("at least one transfer is required"))?;
    // Paused by a rescan of its network, the queue answers 409, as above.
    let transaction_ids = state
        .wallets
        .send_all(&tag, transfers)
        .await?
        .into_iter()
        .map(|id| id.to_string())
        .collect();
    Ok((
        StatusCode::ACCEPTED,
        Json(BatchResponse { transaction_ids }),
    ))
}

/// The wallet a transaction route reads or sends from.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Path)]
struct WalletPath {
    /// The wallet ID a create or watch call returned.
    id: String,
}

/// A canonical address within its network.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct Address {
    /// The address's chain and network.
    network: Network,
    /// Canonical address text.
    value: String,
}

impl From<base::Address> for Address {
    fn from(value: base::Address) -> Self {
        Self {
            network: value.network.into(),
            value: value.value,
        }
    }
}

/// The query string of a transfer submission: neither route takes one, so
/// anything but an absent or empty query is refused before the body is read.
enum SubmitQuery {
    AbsentOrEmpty,
}

impl<S> axum::extract::FromRequestParts<S> for SubmitQuery
where
    S: Send + Sync,
{
    type Rejection = Error;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        if parts.uri.query().is_some_and(|query| !query.is_empty()) {
            return Err(Error::invalid_request(
                "transaction query parameters are not supported",
            ));
        }
        Ok(Self::AbsentOrEmpty)
    }
}

/// The canonical destination `text` names in `wallet`'s own asset network.
///
/// # Errors
///
/// A 400 for text the asset's chain refuses.
fn destination(state: &AppState, wallet: &Wallet, text: &str) -> Result<base::Address, Error> {
    state
        .wallets
        .parse(wallet.asset()?, text)
        .map_err(Error::by_asset)
}

/// The single and the batch wire amount: exact display units or the explicit
/// drain request.
///
/// # Errors
///
/// A 400 for text that is neither an exact base-10 decimal nor `all`.
fn amount(text: &str) -> Result<wallets::Amount, Error> {
    if text == "all" {
        return Ok(wallets::Amount::All);
    }
    text.parse::<base::Decimal>()
        .map(wallets::Amount::Exact)
        .map_err(|_| Error::invalid_request("amount must be an exact base-10 decimal or all"))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use base::{BlockHash, BlockHeight, BlockRef, Decimal};
    use indexing::WalletRegistry as _;
    use indexing::{AssetId, ChainName, MovementId, Network, TransactionRef};
    use serde_json::json;
    use wallets::{
        Amount, WalletErrorKind,
        testing::{Factory, Outcome, Submission},
    };

    use super::*;
    use crate::api::{
        emits_required,
        harness::{App, Reply},
    };

    #[tokio::test]
    async fn history_pages_confirmed_and_pending_transactions() {
        let app = App::ready().await;
        let id = app.wallet_id().await;
        app.mine().await;

        let history = app
            .call(
                "GET",
                &format!("/v1/wallets/{id}/transactions?limit=10"),
                None,
                true,
            )
            .await;

        assert_eq!(history.status, StatusCode::OK);
        let history = history.json();
        assert_eq!(history["checkpoint"]["height"], 8);
        assert_eq!(history["transactions"][0]["transaction_id"], "payout-7");
        assert_eq!(history["transactions"][0]["status"]["kind"], "confirmed");
        assert_eq!(history["transactions"][0]["status"]["confirmations"], 3);
        assert_eq!(history["pending"][0]["transaction_id"], "payout-9");
        assert_eq!(history["pending"][0]["status"]["kind"], "pending");
        assert_eq!(history["pending"][0]["status"]["block"]["height"], 9);
        assert_eq!(
            history["completeness"], "incomplete",
            "a page states its completeness in the same words the balance does"
        );
        assert_eq!(history["transactions"][0]["movements"][0]["amount"], "1.5");
    }

    /// Included in block one, as the projected pages are.
    fn included() -> indexing::Status {
        indexing::Status::Included {
            block: BlockRef {
                position: base::BlockPosition(1),
                height: BlockHeight(1),
                hash: BlockHash(vec![1]),
                parent: None,
                timestamp: None,
            },
        }
    }

    #[test]
    fn page_models_require_exactly_what_they_emit() {
        let network = crate::models::Network {
            chain: "fixture".to_owned(),
            name: "regtest".to_owned(),
        };
        let asset = Asset {
            chain: "fixture".to_owned(),
            id: "native".to_owned(),
            name: None,
            ticker: None,
            decimals: 8,
        };
        let movement = Movement {
            id: "movement-0".to_owned(),
            kind: MovementKind::Transfer,
            asset: asset.clone(),
            amount: "1.5".to_owned(),
            from: None,
            to: Some(Address {
                network: network.clone(),
                value: "fixture-1".to_owned(),
            }),
        };
        let fee = Fee {
            asset,
            amount: "0.1".to_owned(),
            payer: None,
        };
        emits_required("Movement", &movement);
        emits_required("Fee", &fee);
        emits_required(
            "TransactionPage",
            HistoryResponse {
                checkpoint: None,
                transactions: Vec::new(),
                pending: Vec::new(),
                next_cursor: None,
                completeness: Completeness::Overdrawn,
            },
        );
        let block = serde_json::json!({
            "position": 7, "height": 7, "hash": "ab".repeat(32), "parent": null
        });
        emits_required(
            "Transaction",
            Transaction {
                network,
                transaction_id: "tx-0".to_owned(),
                status: serde_json::from_value(serde_json::json!({
                    "kind": "failed", "block": block, "reason": null,
                }))
                .expect("failed status"),
                movements: vec![movement],
                fee: Some(fee),
            },
        );
    }

    fn network() -> Network {
        Network {
            chain: ChainName::new("bitcoin"),
            name: "regtest".to_owned(),
        }
    }

    fn address(value: &str) -> base::Address {
        base::Address {
            network: network(),
            value: value.to_owned(),
        }
    }

    /// Metadata of the example chain's 8-decimal native asset.
    const COIN: base::Asset = base::Asset::new(
        base::ChainNetwork::new(
            base::ChainName::from_static("bitcoin"),
            "regtest",
            base::NetworkKind::Testnet,
            "BTC",
        ),
        "Bitcoin",
        "BTC",
        8,
    );

    /// What the chain declares: its own currency at 8 decimals, and one
    /// 6-decimal token of the same network.
    fn units() -> base::Units {
        base::Units::from_iter([
            (
                (network(), AssetId::native(network().chain)),
                base::Unit::native(&COIN),
            ),
            ((network(), token()), base::Unit::token(6)),
        ])
    }

    fn token() -> AssetId {
        AssetId {
            chain: network().chain,
            asset: "token".to_owned(),
        }
    }

    /// A history read that answers every query with one page.
    struct Answering(indexing::TransactionPage);

    impl indexing::History for Answering {
        fn history<'a>(
            &'a self,
            _of: &'a dyn indexing::Holder,
            _page: indexing::Page,
        ) -> indexing::BoxFuture<'a, Result<indexing::TransactionPage, indexing::IndexError>>
        {
            Box::pin(async move { Ok(self.0.clone()) })
        }
    }

    /// What the history handler answers when the index reads `indexed` for a
    /// Bitcoin regtest wallet of `asset`, under the units of [`units`].
    async fn project(
        indexed: indexing::TransactionPage,
        asset: &AssetId,
    ) -> Result<HistoryResponse, Error> {
        let factory = Factory::new(network());
        let factory = if asset.is_native() {
            factory
        } else {
            factory.token(asset.asset.clone())
        };
        let app = App::without_usdc(factory).await;
        let registration = indexing::Registration {
            id: "wallet-1".to_owned(),
            address: address("addr-07"),
            asset: asset.clone(),
            label: None,
            secret: None,
        };
        app.state
            .registry
            .register(registration)
            .await
            .expect("a registered wallet");
        let state = AppState {
            history: Arc::new(Answering(indexed)),
            units: Arc::new(units()),
            ..app.state.clone()
        };
        let path = WalletPath {
            id: "wallet-1".to_owned(),
        };
        let query = HistoryQuery {
            cursor: None,
            limit: 10,
        };
        let Json(page) = history(State(Arc::new(state)), Path(path), Ok(Query(query))).await?;
        Ok(page)
    }

    /// The projection is the whole of the edge: the index answers in atomic
    /// units and the asset's decimals — which it never learns — turn them
    /// into the same wire body the three-model path produced.
    #[tokio::test]
    async fn history_conversion_scales_atomic_amounts_and_preserves_typed_facts() {
        let block = BlockRef {
            position: base::BlockPosition(12),
            height: BlockHeight(12),
            hash: BlockHash(vec![0xab; 32]),
            parent: Some(base::BlockParent {
                position: base::BlockPosition(11),
                hash: BlockHash(vec![0xcd; 32]),
            }),
            timestamp: Some(44),
        };
        let native = AssetId::native(network().chain);
        let transaction = indexing::Transaction {
            id: TransactionRef {
                network: network(),
                id: "tx-1".to_owned().into(),
            },
            movements: vec![
                indexing::ValueMovement::input(
                    MovementId("input:0".to_owned()),
                    native.clone(),
                    Decimal::from(123_000_000_u64),
                    Some(address("sender")),
                ),
                indexing::ValueMovement::output(
                    MovementId("output:0".to_owned()),
                    native.clone(),
                    Decimal::from(122_000_000_u64),
                    Some(address("recipient")),
                ),
            ],
            fee: Some(indexing::NetworkFee {
                asset: native.clone(),
                amount: Decimal::from(1_000_000_u64),
                payer: Some(address("sender")),
            }),
            order: 0,
            status: indexing::Status::Included {
                block: block.clone(),
            },
        };
        let indexed = indexing::TransactionPage {
            checkpoint: Some(block.clone()),
            pending: Vec::new(),
            transactions: vec![indexing::ObservedTransaction {
                transaction,
                status: indexing::TransactionStatus::Confirmed {
                    block,
                    confirmations: 7,
                },
            }],
            next: None,
            completeness: indexing::Completeness::Incomplete,
        };

        let page = project(indexed, &native).await.expect("projected history");
        let transaction = &page.transactions[0];
        assert_eq!(page.checkpoint.as_ref().map(|value| value.height), Some(12));
        assert_eq!(transaction.network.name, "regtest");
        assert_eq!(transaction.transaction_id, "tx-1");
        assert_eq!(transaction.movements.len(), 2);
        assert_eq!(transaction.movements[0].amount, "1.23");
        assert_eq!(transaction.movements[0].asset.decimals, 8);
        assert_eq!(transaction.movements[0].to, None);
        assert_eq!(transaction.movements[1].kind, MovementKind::Output);
        assert_eq!(transaction.fee.as_ref().expect("fee").amount, "0.01");
        assert!(matches!(
            transaction.status,
            Status::Confirmed {
                confirmations: 7,
                ..
            }
        ));
        let status = serde_json::to_value(&transaction.status).expect("status serializes");
        assert_eq!(status["kind"], "confirmed");
        assert_eq!(status["confirmations"], 7);
        assert_eq!(status["block"]["position"], 12);
        assert_eq!(status["block"]["height"], 12);
        assert_eq!(status["block"]["parent"]["position"], 11);
        assert_eq!(status["block"]["parent"]["hash"], "cd".repeat(32));
        assert!(status["block"].get("parent_hash").is_none());
        assert!(status["block"].get("block").is_none());
        assert!(status["block"].get("timestamp").is_none());
        assert!(status.get("proof").is_none());
    }

    /// Each status's whole body, byte for byte: its fields, their order, and
    /// the `null` a failure without a reason still writes.
    ///
    /// Schema generation cannot prove exact wire bytes or deserialization.
    #[test]
    fn each_status_is_the_same_bytes_and_reads_back_as_itself() {
        let block = BlockRef {
            position: base::BlockPosition(12),
            height: BlockHeight(12),
            hash: BlockHash(vec![0xab; 32]),
            parent: Some(base::BlockParent {
                position: base::BlockPosition(11),
                hash: BlockHash(vec![0xcd; 32]),
            }),
            timestamp: Some(44),
        };
        let included = format!(
            r#"{{"position":12,"height":12,"hash":"{}","parent":{{"position":11,"hash":"{}"}}}}"#,
            "ab".repeat(32),
            "cd".repeat(32),
        );
        for (status, tail) in [
            (
                indexing::TransactionStatus::Pending {
                    block: block.clone(),
                    confirmations: 1,
                },
                r#""kind":"pending","confirmations":1"#,
            ),
            (
                indexing::TransactionStatus::Confirmed {
                    block: block.clone(),
                    confirmations: 7,
                },
                r#""kind":"confirmed","confirmations":7"#,
            ),
            (
                indexing::TransactionStatus::Failed {
                    block: block.clone(),
                    reason: Some("out of gas".to_owned()),
                },
                r#""kind":"failed","reason":"out of gas""#,
            ),
            (
                indexing::TransactionStatus::Failed {
                    block: block.clone(),
                    reason: None,
                },
                r#""kind":"failed","reason":null"#,
            ),
        ] {
            let (kind, fact) = tail.split_once(',').expect("a kind and one fact");
            let status = Status::from(status);

            let body = serde_json::to_string(&status).expect("status serializes");

            assert_eq!(body, format!(r#"{{{kind},"block":{included},{fact}}}"#));
            assert_eq!(
                serde_json::from_str::<Status>(&body).expect("status reads back"),
                status,
                "{kind} reads back as the member that wrote it"
            );
            let mut extra = serde_json::to_value(&status).expect("wire value");
            extra["unexpected"] = true.into();
            assert!(serde_json::from_value::<Status>(extra).is_err());
        }
    }

    /// A page that left the query is refused whole, before any of it is
    /// converted: another network's transaction, an asset the query did not
    /// ask for, or a fee priced in anything but the chain's own currency.
    #[tokio::test]
    async fn a_page_outside_the_query_is_refused_before_conversion() {
        let elsewhere = Network {
            chain: ChainName::new("bitcoin"),
            name: "mainnet".to_owned(),
        };
        let native = AssetId::native(network().chain);
        let transaction = |network: &Network, asset: AssetId, fee: Option<AssetId>| {
            indexing::ObservedTransaction {
                transaction: indexing::Transaction {
                    id: TransactionRef {
                        network: network.clone(),
                        id: "tx-1".to_owned().into(),
                    },
                    movements: vec![indexing::ValueMovement::mint(
                        MovementId("mint".to_owned()),
                        asset,
                        Decimal::from(1_u64),
                        address("recipient"),
                    )],
                    fee: fee.map(|asset| indexing::NetworkFee {
                        asset,
                        amount: Decimal::from(1_u64),
                        payer: None,
                    }),
                    order: 0,
                    status: included(),
                },
                status: indexing::TransactionStatus::Failed {
                    block: BlockRef {
                        position: base::BlockPosition(1),
                        height: BlockHeight(1),
                        hash: BlockHash(vec![1]),
                        parent: None,
                        timestamp: None,
                    },
                    reason: None,
                },
            }
        };
        let page = |transactions| indexing::TransactionPage {
            checkpoint: None,
            transactions,
            pending: Vec::new(),
            next: None,
            completeness: indexing::Completeness::Incomplete,
        };

        for (refused, reason) in [
            (
                page(vec![transaction(&elsewhere, native.clone(), None)]),
                "a transaction of another network",
            ),
            (
                page(vec![transaction(&network(), token(), None)]),
                "an asset outside the query",
            ),
            (
                page(vec![transaction(&network(), native.clone(), Some(token()))]),
                "a fee that is not the chain's own currency",
            ),
        ] {
            project(refused, &native).await.expect_err(reason);
        }
    }

    /// A token's amounts scale by the token's own decimals and its fees by the
    /// chain's currency, in one page: the two scales are resolved per asset,
    /// by the identity each amount already carries.
    #[tokio::test]
    async fn a_token_page_scales_each_amount_by_its_own_assets_unit() {
        let indexed = indexing::TransactionPage {
            checkpoint: None,
            pending: Vec::new(),
            transactions: vec![indexing::ObservedTransaction {
                transaction: indexing::Transaction {
                    id: TransactionRef {
                        network: network(),
                        id: "tx-1".to_owned().into(),
                    },
                    movements: vec![indexing::ValueMovement::transfer(
                        MovementId("transfer:0".to_owned()),
                        token(),
                        Decimal::from(1_500_000_u64),
                        address("sender"),
                        address("recipient"),
                    )],
                    fee: Some(indexing::NetworkFee {
                        asset: AssetId::native(network().chain),
                        amount: Decimal::from(1_000_000_u64),
                        payer: Some(address("sender")),
                    }),
                    order: 0,
                    status: included(),
                },
                status: indexing::TransactionStatus::Confirmed {
                    block: BlockRef {
                        position: base::BlockPosition(1),
                        height: BlockHeight(1),
                        hash: BlockHash(vec![1]),
                        parent: None,
                        timestamp: None,
                    },
                    confirmations: 1,
                },
            }],
            next: None,
            completeness: indexing::Completeness::Incomplete,
        };

        let page = project(indexed, &token())
            .await
            .expect("projected token history");

        let transaction = &page.transactions[0];
        assert_eq!(transaction.movements[0].amount, "1.5");
        assert_eq!(transaction.movements[0].asset.decimals, 6);
        assert_eq!(transaction.movements[0].asset.id, "token");
        assert_eq!(transaction.movements[0].asset.ticker, None);
        let fee = transaction.fee.as_ref().expect("fee");
        assert_eq!(fee.amount, "0.01");
        assert_eq!(fee.asset.decimals, 8);
        assert_eq!(fee.asset.ticker.as_deref(), Some("BTC"));
    }

    /// The cursor is a position and nothing else. It used to carry the
    /// checkpoint it was issued at, so the index could refuse one whose
    /// checkpoint had moved; the confirmed store is append-only now, so that
    /// refusal is gone and so is the field.
    #[test]
    fn a_history_cursor_carries_only_its_position() {
        use base64::Engine;

        let position = indexing::HistoryPosition {
            height: BlockHeight(9),
            transaction_order: 2,
            transaction: TransactionRef {
                network: network(),
                id: "tx-1".to_owned().into(),
            },
        };

        let encoded = Cursor::encode(&position).expect("cursor encodes");
        let decoded = Cursor::decode(&encoded).expect("cursor decodes");

        assert_eq!(decoded, position);
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .expect("base64 cursor");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).expect("JSON cursor"),
            serde_json::json!({
                "chain": "bitcoin",
                "network": "regtest",
                "transaction": "tx-1",
                "height": 9,
                "order": 2
            })
        );
    }

    fn cursor_wire() -> serde_json::Value {
        serde_json::json!({
            "chain": "bitcoin",
            "network": "regtest",
            "transaction": "tx-1",
            "height": 9,
            "order": 2
        })
    }

    fn encode_cursor_wire(wire: &serde_json::Value) -> String {
        use base64::Engine;

        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(wire).expect("cursor serializes"))
    }

    #[test]
    fn history_cursor_rejects_unknown_fields() {
        let valid = cursor_wire();
        assert!(Cursor::decode(&encode_cursor_wire(&valid)).is_ok());

        let mut invalid = valid.clone();
        invalid["unexpected"] = true.into();
        assert!(Cursor::decode(&encode_cursor_wire(&invalid)).is_err());
    }

    /// A cursor issued before the page carried native block order names a
    /// height and a transaction but no place inside the block, so it cannot
    /// say where to resume once two transactions of one block are stored.
    ///
    /// It is refused rather than defaulted to zero, which would resume *below*
    /// every transaction of that block and hand the caller its own last page
    /// again. The project is pre-release and no such cursor was ever issued to
    /// anyone; the refusal is here so the break is deliberate rather than a
    /// silent reinterpretation.
    #[test]
    fn history_cursor_rejects_a_cursor_without_a_place_in_its_block() {
        let mut wire = cursor_wire();
        assert!(
            wire.as_object_mut()
                .expect("a cursor object")
                .remove("order")
                .is_some()
        );

        assert!(
            Cursor::decode(&encode_cursor_wire(&wire)).is_err(),
            "a cursor with no transaction order cannot name a position"
        );
    }

    /// A cursor issued before this change carries a checkpoint the decoder no
    /// longer knows. It is refused rather than silently reinterpreted, so a
    /// client restarts its paging instead of resuming at a position that was
    /// never checked.
    #[test]
    fn history_cursor_rejects_a_cursor_that_still_carries_a_checkpoint() {
        let mut wire = cursor_wire();
        wire["checkpoint"] = serde_json::json!({
            "block": {
                "position": 42,
                "height": 12,
                "hash": "abab",
                "parent": { "position": 40, "hash": "cdcd" }
            },
            "timestamp": 44
        });

        assert!(
            Cursor::decode(&encode_cursor_wire(&wire)).is_err(),
            "a checkpoint-bound cursor must not decode after the confirmed-line cutover"
        );
    }

    #[tokio::test]
    async fn a_single_transfer_reaches_its_wallet_send() {
        let app = App::ready().await;
        let id = app.wallet_id().await;

        let submitted = app
            .call(
                "POST",
                &format!("/v1/wallets/{id}/transactions"),
                Some(json!({"destination": "ADDR-DESTINATION", "amount": "2.25"})),
                true,
            )
            .await;

        assert_eq!(submitted.status, StatusCode::ACCEPTED);
        assert_eq!(submitted.json(), json!({"transaction_id": "single"}));
        assert_eq!(
            app.sent(),
            [Submission::Single(
                App::destination(),
                "2.25".parse().expect("an amount")
            )]
        );
    }

    #[tokio::test]
    async fn a_refused_single_transfer_answers_the_wallets_refusal() {
        let app = App::ready().await;
        let id = app.wallet_id().await;
        app.bitcoin_outbox()
            .answer(Outcome::Refused(WalletErrorKind::Transaction));

        let refused = app
            .call(
                "POST",
                &format!("/v1/wallets/{id}/transactions"),
                Some(json!({"destination": "addr-destination", "amount": "1"})),
                true,
            )
            .await;

        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            refused.json(),
            json!({"message": "the wallet double refused this send"})
        );
        assert_eq!(app.sent().len(), 1, "the refusal came from the send itself");
    }

    #[tokio::test]
    async fn destinations_are_parsed_by_the_wallet_chain_before_any_send() {
        let app = App::ready().await;
        let wallet_id = app.wallet_id().await;
        let single = app
            .call(
                "POST",
                &format!("/v1/wallets/{wallet_id}/transactions"),
                Some(json!({"destination": "not-an-address", "amount": "1"})),
                true,
            )
            .await;
        assert_eq!(single.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            single.json(),
            json!({"message": "a wallet double address is `addr-` and more text"})
        );

        let batch = app
            .call(
                "POST",
                "/v1/transactions",
                Some(json!({"transfers": [
                    {"wallet_id": wallet_id, "destination": "addr-destination", "amount": "1"},
                    {"wallet_id": wallet_id, "destination": "not-an-address", "amount": "1"}
                ]})),
                true,
            )
            .await;
        assert_eq!(batch.status, StatusCode::BAD_REQUEST);
        assert_eq!(batch.json()["failed_index"], 1);
        assert!(app.sent().is_empty());
    }

    #[tokio::test]
    async fn all_is_preserved_for_single_and_batch_transfers() {
        let app = App::ready().await;
        let id = app.wallet_id().await;
        let transfer = json!({"destination": "addr-destination", "amount": "all"});
        let single = app
            .call(
                "POST",
                &format!("/v1/wallets/{id}/transactions"),
                Some(transfer),
                true,
            )
            .await;
        assert_eq!(single.status, StatusCode::ACCEPTED);
        let body = json!({"transfers": [{
            "wallet_id": id,
            "destination": "addr-destination",
            "amount": "all"
        }]});
        let batch = app.call("POST", "/v1/transactions", Some(body), true).await;
        assert_eq!(batch.status, StatusCode::ACCEPTED);
        assert_eq!(
            app.sent(),
            [
                Submission::Single(App::destination(), Amount::All),
                Submission::Batch(vec![(App::destination(), Amount::All)]),
            ]
        );
    }

    #[tokio::test]
    async fn a_batch_reaches_its_assets_sender() {
        let app = App::ready().await;
        let id = app.wallet_id().await;

        let batch = app
            .call(
                "POST",
                "/v1/transactions",
                Some(json!({"transfers": [{
                    "wallet_id": id,
                    "destination": "addr-destination",
                    "amount": "3"
                }]})),
                true,
            )
            .await;

        assert_eq!(batch.status, StatusCode::ACCEPTED);
        assert_eq!(batch.json(), json!({"transaction_ids": ["batch"]}));
        assert_eq!(
            app.sent(),
            [Submission::Batch(vec![(
                App::destination(),
                "3".parse().expect("an amount")
            )])]
        );
    }

    #[tokio::test]
    async fn an_ambiguous_batch_answers_503_with_its_local_transaction_id() {
        let app = App::ready().await;
        let id = app.wallet_id().await;
        app.bitcoin_outbox().answer(Outcome::Ambiguous);

        let batch = app
            .call(
                "POST",
                "/v1/transactions",
                Some(json!({"transfers": [{
                    "wallet_id": id,
                    "destination": "addr-destination",
                    "amount": "3"
                }]})),
                true,
            )
            .await;

        assert_eq!(batch.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            batch.json(),
            json!({
                "message": "the wallet double cannot tell whether this send was accepted",
                "ambiguous_transaction_id": "ambiguous"
            })
        );
        assert_eq!(app.sent().len(), 1);
    }

    #[tokio::test]
    async fn transaction_query_rejection_follows_auth_and_precedes_body_semantics() {
        let app = App::ready().await;
        let unauthorized = app
            .raw(
                "POST",
                "/v1/transactions?commitment=finalized",
                Some("{"),
                false,
                &[],
            )
            .await;
        assert_eq!(unauthorized.status, StatusCode::UNAUTHORIZED);

        let wallet_id = app.wallet_id().await;
        let malformed = app
            .raw(
                "POST",
                &format!("/v1/wallets/{wallet_id}/transactions?commitment=finalized"),
                Some("{"),
                true,
                &[],
            )
            .await;
        assert_transaction_query_rejected(&malformed);

        let empty = app
            .call(
                "POST",
                "/v1/transactions?min_context_slot=9",
                Some(json!({"transfers": []})),
                true,
            )
            .await;
        assert_transaction_query_rejected(&empty);

        let transfers = (0..51)
            .map(|_| {
                json!({
                    "wallet_id": wallet_id,
                    "destination": "addr-destination",
                    "amount": "1"
                })
            })
            .collect::<Vec<_>>();
        let oversized = app
            .call(
                "POST",
                "/v1/transactions?priority_fee=1",
                Some(json!({"transfers": transfers})),
                true,
            )
            .await;
        assert_transaction_query_rejected(&oversized);
        assert!(app.sent().is_empty());
    }

    #[tokio::test]
    async fn empty_query_and_unrecognized_headers_do_not_change_transaction_semantics() {
        let app = App::ready().await;
        let wallet_id = app.wallet_id().await;
        let single_body = json!({
            "destination": "addr-destination",
            "amount": "2.25"
        })
        .to_string();
        let single = app
            .raw(
                "POST",
                &format!("/v1/wallets/{wallet_id}/transactions?"),
                Some(&single_body),
                true,
                &[("x-min-context-slot", "9"), ("traceparent", "test-trace")],
            )
            .await;
        assert_eq!(single.status, StatusCode::ACCEPTED);
        assert_eq!(single.json(), json!({"transaction_id": "single"}));

        let batch_body = json!({"transfers": [{
            "wallet_id": wallet_id,
            "destination": "addr-destination",
            "amount": "3"
        }]})
        .to_string();
        let batch = app
            .raw(
                "POST",
                "/v1/transactions?",
                Some(&batch_body),
                true,
                &[("x-retry-transaction", "true")],
            )
            .await;
        assert_eq!(batch.status, StatusCode::ACCEPTED);
        assert_eq!(batch.json(), json!({"transaction_ids": ["batch"]}));

        assert_eq!(
            app.sent(),
            [
                Submission::Single(App::destination(), "2.25".parse().expect("an amount")),
                Submission::Batch(vec![(App::destination(), "3".parse().expect("an amount"))]),
            ]
        );
    }

    #[tokio::test]
    async fn batch_wire_maximum_precedes_conversion_and_leaves_minimum_to_the_sdk() {
        let app = App::ready().await;

        let empty = app
            .call(
                "POST",
                "/v1/transactions",
                Some(json!({"transfers": []})),
                true,
            )
            .await;
        assert_eq!(empty.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            empty.json(),
            json!({"message": "at least one transfer is required"})
        );

        let transfers = (0..=wallets::MAX_TRANSFERS)
            .map(|_| {
                json!({
                    "wallet_id": "missing-wallet",
                    "destination": "addr-destination",
                    "amount": "not-a-decimal"
                })
            })
            .collect::<Vec<_>>();
        let oversized = app
            .call(
                "POST",
                "/v1/transactions",
                Some(json!({
                    "transfers": transfers
                })),
                true,
            )
            .await;
        assert_eq!(oversized.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            oversized.json(),
            json!({"message": "at most 50 transfers are allowed"})
        );
        assert!(app.sent().is_empty());

        let wallet_id = app.wallet_id().await;
        for admitted_count in [1, wallets::MAX_TRANSFERS] {
            let destination = json!("addr-destination");
            let transfers = (0..admitted_count)
                .map(|_| {
                    json!({
                        "wallet_id": wallet_id,
                        "destination": destination,
                        "amount": "1"
                    })
                })
                .collect::<Vec<_>>();
            let admitted = app
                .call(
                    "POST",
                    "/v1/transactions",
                    Some(json!({"transfers": transfers})),
                    true,
                )
                .await;
            assert_eq!(admitted.status, StatusCode::ACCEPTED);
            assert_eq!(admitted.json(), json!({"transaction_ids": ["batch"]}));
        }

        let sizes = app
            .sent()
            .into_iter()
            .map(|submission| match submission {
                Submission::Batch(sent) => sent.len(),
                Submission::Single(..) => panic!("only batches were sent"),
            })
            .collect::<Vec<_>>();
        assert_eq!(sizes, [1, wallets::MAX_TRANSFERS]);
    }

    #[tokio::test]
    async fn transaction_bodies_reject_unknown_controls_before_sdk_delegation() {
        let app = App::ready().await;
        let wallet_id = app.wallet_id().await;
        let single_path = format!("/v1/wallets/{wallet_id}/transactions");
        let priority_body = json!({
            "transfers": [{
                "wallet_id": wallet_id,
                "destination": "addr-destination",
                "amount": "1"
            }],
            "priority_fee": "1"
        });
        let cases = [
            (
                "single destination lag control",
                single_path.as_str(),
                json!({
                    "destination": {
                        "encoding": "hex",
                        "text": "addr-destination",
                        "max_lag": 4
                    },
                    "amount": "1"
                }),
            ),
            (
                "batch destination reference control",
                "/v1/transactions",
                json!({"transfers": [{
                    "wallet_id": wallet_id,
                    "destination": {
                        "encoding": "hex",
                        "text": "addr-destination",
                        "reference_slot": 9
                    },
                    "amount": "1"
                }]}),
            ),
            (
                "single commitment control",
                single_path.as_str(),
                json!({
                    "destination": "addr-destination",
                    "amount": "1",
                    "commitment": "finalized"
                }),
            ),
            (
                "single retry control",
                single_path.as_str(),
                json!({
                    "destination": "addr-destination",
                    "amount": "1",
                    "retry": true
                }),
            ),
            (
                "batch item Memo override",
                "/v1/transactions",
                json!({"transfers": [{
                    "wallet_id": wallet_id,
                    "destination": "addr-destination",
                    "amount": "1",
                    "memo": "caller-selected"
                }]}),
            ),
            ("batch priority control", "/v1/transactions", priority_body),
        ];

        for (case, path, body) in cases {
            let response = app.call("POST", path, Some(body), true).await;
            assert_eq!(response.status, StatusCode::BAD_REQUEST, "{case}");
            assert_eq!(
                response.json(),
                json!({"message": "request body must match the documented JSON schema"}),
                "{case}"
            );
            assert!(app.sent().is_empty());
        }
    }

    #[tokio::test]
    async fn malformed_batch_amount_precedes_wallet_lookup_and_signing() {
        let app = App::ready().await;
        let watched = app
            .call(
                "POST",
                "/v1/wallets",
                Some(json!({"asset": "btc", "address": "addr-watched"})),
                true,
            )
            .await;
        let watched_id = watched.json()["id"].as_str().expect("watch").to_owned();
        for id in ["missing-wallet", watched_id.as_str()] {
            let body = json!({"transfers": [{
                "wallet_id": id, "destination": "addr-destination", "amount": "invalid"
            }]});
            let response = app.call("POST", "/v1/transactions", Some(body), true).await;
            assert_eq!(response.status, StatusCode::BAD_REQUEST);
            assert_eq!(response.json()["failed_index"], 0);
            assert_eq!(
                response.json()["message"],
                "amount must be an exact base-10 decimal or all"
            );
        }
        assert!(app.sent().is_empty());
    }

    fn assert_transaction_query_rejected(response: &Reply) {
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert_eq!(
            response.json(),
            json!({"message": "transaction query parameters are not supported"})
        );
    }
}
