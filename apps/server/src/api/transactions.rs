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

use indexing::{Entry, Position, Standing};

use super::{Error, ErrorBody};
use crate::{
    asset::AssetTag,
    models::{Asset, Block, Network, Wallet},
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
    /// A configured asset tag of the wallet's network: the asset whose
    /// history is read.
    #[param(value_type = String, pattern = "^[a-z0-9][a-z0-9-]{0,31}$")]
    asset: AssetTag,
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

#[utoipa::path(
    get,
    path = "/v1/wallets/{id}/transactions",
    operation_id = "read_wallet_transactions",
    summary = "Read one page of a wallet's history",
    description = "The wallet's canonical history in `asset`; a fee is shown where this \
        address was charged it. `transactions` \
        is confirmed and append-only, so its cursor stays valid however far the checkpoint \
        moves; `pending` carries the transactions above the confirmed line and is populated \
        on the first page only; `behind` says how many blocks the address's inspection trails \
        the confirmed line, as the balance does.",
    params(WalletPath, HistoryQuery),
    security(("bearerAuth" = [])),
    responses(
        (status = 200, description = "One history page", body = HistoryResponse),
        (status = 400, description = "Malformed query, cursor, or limit", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Wallet is unknown, or the asset is not of its network",
            body = ErrorBody),
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
    if query.limit == 0 || query.limit > 1_000 {
        return Err(Error::invalid_request(
            "history limit must be between 1 and 1000",
        ));
    }
    let after = query.cursor.as_deref().map(Cursor::decode).transpose()?;
    let wallet = Wallet::find(&state.payments, &path.id).await?;
    let subject = &wallet.subject(&state.payments.wallets, &query.asset)?;
    // The pending blocks are sampled before the rows are read, and narrowed
    // by the line the rows were read at, so a commit landing in between is
    // listed once. The page is in atomic units; each asset's declared unit
    // scales it below.
    let pending = state
        .payments
        .pending
        .iter()
        .find(|(network, _)| *network == subject.address.network)
        .map(|(_, pending)| pending.clone())
        .ok_or_else(|| {
            indexing::IndexError::new(
                indexing::IndexErrorKind::NetworkMismatch,
                "index network is not configured",
                false,
            )
        })?;
    let sample = pending.sampled()?;
    // One snapshot: the repository's own read.
    let page = state
        .repo
        .history(subject, after.as_ref(), query.limit)
        .await?
        .completed(subject, after.is_none(), sample)?;
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
            .payments
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
    let transaction = |observed: Entry| {
        let transaction = observed.transaction;
        Ok::<_, Error>(Transaction {
            network: transaction.id.network.clone().into(),
            transaction_id: transaction.id.id.to_string(),
            status: observed.standing.into(),
            movements: transaction
                .movements
                .into_iter()
                .map(&movement)
                .collect::<Result<_, Error>>()?,
            fee: transaction.fee.map(&fee).transpose()?,
        })
    };
    let convert = |observed: Vec<Entry>| {
        observed
            .into_iter()
            .map(&transaction)
            .collect::<Result<Vec<_>, Error>>()
    };
    Ok(Json(HistoryResponse {
        next_cursor: page.next.as_ref().map(Cursor::encode).transpose()?,
        behind: page.behind,
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

impl From<Standing> for Status {
    fn from(value: Standing) -> Self {
        match value {
            Standing::Pending {
                block,
                confirmations,
            } => Self::Pending {
                block: block.into(),
                confirmations,
            },
            Standing::Confirmed {
                block,
                confirmations,
            } => Self::Confirmed {
                block: block.into(),
                confirmations,
            },
            Standing::Failed { block, reason } => Self::Failed {
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
    /// How many blocks this address's inspection is behind the confirmed
    /// line; 0 when caught up. `next_cursor` cannot carry it — that speaks
    /// about what follows the page, and a backfill inserts rows *before* it —
    /// so a caller paging to the end mid-backfill reads a non-zero `behind`
    /// rather than a complete export.
    behind: u64,
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
    fn encode(position: &Position) -> Result<String, Error> {
        use base64::Engine;

        let bytes = serde_json::to_vec(&Self {
            chain: position.transaction.network.chain.to_string(),
            network: position.transaction.network.name.clone(),
            transaction: position.transaction.id.to_string(),
            height: position.height.0,
            order: position.order,
        })
        .map_err(|_| Self::invalid_response("history cursor could not be encoded"))?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
    }

    fn decode(value: &str) -> Result<Position, Error> {
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
        Ok(Position {
            height: base::BlockHeight(cursor.height),
            order: cursor.order,
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
    /// A configured asset tag of the wallet's network: the asset sent.
    #[schema(value_type = String, pattern = "^[a-z0-9][a-z0-9-]{0,31}$")]
    asset: AssetTag,
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
    description = "Submits one transfer of `asset` from the wallet. Every asset \
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
    let wallet = Wallet::find(&state.payments, &path.id).await?;
    wallet.subject(&state.payments.wallets, &request.asset)?;
    // The parse only produces addresses of the asset's network, which is the
    // wallet's, so no separate same-network check exists.
    let amount = amount(&request.amount)?;
    let destination = destination(&state, &request.asset, &request.destination)?;
    // Sending is the asset's capability, not the wallet's: an asset this
    // deployment only receives has no author to hand out, and that one
    // absence is the whole refusal. An archived wallet sends nothing (409).
    let signer = wallet.signer(&state.payments.wallets, &request.asset)?;
    let author = state.payments.wallets.author(&request.asset, &signer)?;
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
    /// The configured asset tag every transfer sends.
    #[schema(value_type = String, pattern = "^[a-z0-9][a-z0-9-]{0,31}$")]
    asset: AssetTag,
    /// Transfers of `asset`, in execution order.
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
    if request.transfers.is_empty() {
        return Err(Error::invalid_request("at least one transfer is required"));
    }
    let tag = request.asset;
    let mut transfers = Vec::with_capacity(request.transfers.len());
    for (failed_index, item) in request.transfers.into_iter().enumerate() {
        let at = |error: Error| error.at(failed_index);
        let amount = amount(&item.amount).map_err(at)?;
        let found = Wallet::find(&state.payments, &item.wallet_id)
            .await
            .map_err(|error| at(error.into()))?;
        found
            .subject(&state.payments.wallets, &tag)
            .map_err(|error| at(error.into()))?;
        let wallet = found
            .signer(&state.payments.wallets, &tag)
            .map_err(|error| at(error.into()))?;
        let to = destination(&state, &tag, &item.destination).map_err(at)?;
        transfers.push(::wallets::Transfer { wallet, to, amount });
    }
    // Paused by a rescan of its network, the queue answers 409, as above.
    let transaction_ids = state
        .payments
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
fn destination(state: &AppState, tag: &AssetTag, text: &str) -> Result<base::Address, Error> {
    state
        .payments
        .wallets
        .parse(tag, text)
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
