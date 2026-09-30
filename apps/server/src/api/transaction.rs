use axum::{
    Json,
    extract::{FromRequestParts, Path, State, rejection::JsonRejection},
    http::{StatusCode, request::Parts},
};
use serde::{Deserialize, Serialize};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{
    Custody, Payments,
    contract::WalletPath,
    error::{Error, ErrorBody},
};

pub(super) fn routes() -> OpenApiRouter<Payments> {
    OpenApiRouter::new()
        .routes(routes!(send))
        .routes(routes!(send_batch))
}

enum SubmitQuery {
    AbsentOrEmpty,
}

impl<S> FromRequestParts<S> for SubmitQuery
where
    S: Send + Sync,
{
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        if parts.uri.query().is_some_and(|query| !query.is_empty()) {
            return Err(Error::invalid_request(
                "transaction query parameters are not supported",
            ));
        }
        Ok(Self::AbsentOrEmpty)
    }
}

/// `POST /v1/wallets/{id}/transactions` body: one transfer from the path's wallet.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Transfer {
    /// Recipient address text, as the wallet's chain writes it.
    destination: String,
    /// Exact decimal in display units, or `all` for the available balance after fees.
    amount: String,
}

impl Transfer {
    /// Shared single/batch wire amount: exact display units or the explicit drain request.
    pub(super) fn amount(text: &str) -> Result<wallets::Amount, Error> {
        if text == "all" {
            return Ok(wallets::Amount::All);
        }
        text.parse::<base::Decimal>()
            .map(wallets::Amount::Exact)
            .map_err(|_| Error::invalid_request("amount must be an exact base-10 decimal or all"))
    }

    /// The destination parsed by the custody's own asset, and the amount.
    ///
    /// The parse only ever produces addresses of that asset's network, which
    /// is why no separate same-network check exists.
    fn resolve(
        self,
        payments: &Payments,
        custody: &Custody,
    ) -> Result<(base::Address, wallets::Amount), Error> {
        let amount = Self::amount(&self.amount)?;
        let destination = payments.destination(custody, &self.destination)?;
        Ok((destination, amount))
    }
}

/// A submitted, not yet confirmed, transaction.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct TransferAccepted {
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
    request_body = Transfer,
    security(("bearerAuth" = [])),
    responses(
        (status = 202, description = "Transaction submitted", body = TransferAccepted),
        (status = 400, description = "Malformed body, amount, or destination", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Wallet is unknown", body = ErrorBody),
        (status = 422, description = "Deterministic rejection, or a watch-only wallet",
            body = ErrorBody),
        (status = 503, description = "Transaction unavailable or exact-envelope submission \
            outcome ambiguous; ambiguous_transaction_id is present only for an unknown outcome",
            body = ErrorBody)
    ),
    tag = "transactions"
)]
async fn send(
    State(payments): State<Payments>,
    Path(path): Path<WalletPath>,
    _query: SubmitQuery,
    request: Result<Json<Transfer>, JsonRejection>,
) -> Result<(StatusCode, Json<TransferAccepted>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let custody = payments.custody(&path.id).await?;
    let (destination, amount) = request.resolve(&payments, &custody)?;
    // Sending is the asset's capability, not the wallet's: an asset this
    // deployment only receives has no author to hand out, and that one
    // absence is the whole refusal.
    let author = payments.wallets.author(&custody.tag, custody.signing()?)?;
    let id = author.send(destination, amount).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(TransferAccepted {
            transaction_id: id.to_string(),
        }),
    ))
}

/// One ordered item of a [`TransferBatch`].
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BatchedTransfer {
    /// The sending wallet.
    pub(super) wallet_id: String,
    /// Recipient address text, as the wallet's chain writes it.
    pub(super) destination: String,
    /// Exact decimal in display units, or `all` to sweep spendable funds.
    pub(super) amount: String,
}

/// `POST /v1/transactions` body: an ordered batch of one wallet asset.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct TransferBatch {
    /// Transfers of one configured asset, in execution order.
    #[schema(min_items = 1, max_items = 50)]
    transfers: Vec<BatchedTransfer>,
}

impl TransferBatch {
    /// Every transfer resolved to the wallet that signs it and a destination
    /// parsed by that wallet's own asset, plus the one asset tag the batch
    /// belongs to. A failure names the transfer's index.
    async fn resolve(
        self,
        payments: &Payments,
    ) -> Result<(super::AssetTag, Vec<wallets::Transfer>), Error> {
        if self.transfers.len() > wallets::MAX_TRANSFERS {
            return Err(Error::invalid_request(format!(
                "at most {} transfers are allowed",
                wallets::MAX_TRANSFERS
            )));
        }
        let mut tag = None;
        let mut transfers = Vec::with_capacity(self.transfers.len());
        for (failed_index, transfer) in self.transfers.into_iter().enumerate() {
            let item = payments
                .transfer(&transfer)
                .await
                .map_err(|error| error.at(failed_index))?;
            if tag.as_ref().is_some_and(|known| *known != item.0) {
                return Err(Error::invalid_batch(
                    failed_index,
                    "all transfers must use the same wallet asset",
                ));
            }
            tag = Some(item.0);
            transfers.push(item.1);
        }
        let tag = tag.ok_or_else(|| Error::invalid_request("at least one transfer is required"))?;
        Ok((tag, transfers))
    }
}

/// Result of an accepted batch.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
struct BatchAccepted {
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
    request_body = TransferBatch,
    security(("bearerAuth" = [])),
    responses(
        (status = 202, description = "Asset batch submitted", body = BatchAccepted),
        (status = 400, description = "Malformed body, batch size, amount, or destination",
            body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "A batch wallet is unknown", body = ErrorBody),
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
    State(payments): State<Payments>,
    _query: SubmitQuery,
    request: Result<Json<TransferBatch>, JsonRejection>,
) -> Result<(StatusCode, Json<BatchAccepted>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let (tag, transfers) = request.resolve(&payments).await?;
    let transaction_ids = payments
        .wallets
        .send_all(&tag, transfers)
        .await?
        .into_iter()
        .map(|id| id.to_string())
        .collect();
    Ok((
        StatusCode::ACCEPTED,
        Json(BatchAccepted { transaction_ids }),
    ))
}
