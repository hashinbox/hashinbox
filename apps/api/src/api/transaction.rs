use axum::{
    Json,
    extract::{FromRequestParts, Path, Query, State, rejection::JsonRejection},
    http::{StatusCode, request::Parts},
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{
    State as HttpState,
    contract::{AddressInput, WalletPath},
    cursor::HistoryCursor,
    error::{Body, Error},
};

pub fn routes() -> OpenApiRouter<HttpState> {
    OpenApiRouter::new()
        .routes(routes!(read))
        .routes(routes!(send))
        .routes(routes!(send_all))
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Scope {
    pub chain: String,
    pub network: String,
}

impl From<indexing::IndexScope> for Scope {
    fn from(value: indexing::IndexScope) -> Self {
        Self {
            chain: value.chain.0,
            network: value.network,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Address {
    pub scope: Scope,
    pub value: String,
}

impl From<indexing::CanonicalAddress> for Address {
    fn from(value: indexing::CanonicalAddress) -> Self {
        Self {
            scope: value.scope.into(),
            value: value.value,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Asset {
    pub chain: String,
    pub id: String,
    pub name: Option<String>,
    pub ticker: Option<String>,
    pub decimals: u32,
}

impl From<wallets::HistoryAsset> for Asset {
    fn from(value: wallets::HistoryAsset) -> Self {
        Self {
            chain: value.id.chain.0,
            id: value.id.asset,
            name: value.name,
            ticker: value.ticker,
            decimals: value.decimals,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MovementKind {
    Transfer,
    Input,
    Output,
    Mint,
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

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Movement {
    pub id: String,
    pub kind: MovementKind,
    pub asset: Asset,
    pub amount: String,
    pub from: Option<Address>,
    pub to: Option<Address>,
}

impl From<wallets::HistoryMovement> for Movement {
    fn from(value: wallets::HistoryMovement) -> Self {
        Self {
            id: value.id.0,
            kind: value.kind.into(),
            asset: value.asset.into(),
            amount: value.amount.to_string(),
            from: value.from.map(Into::into),
            to: value.to.map(Into::into),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Fee {
    pub asset: Asset,
    pub amount: String,
    pub payer: Option<Address>,
}

impl From<wallets::HistoryFee> for Fee {
    fn from(value: wallets::HistoryFee) -> Self {
        Self {
            asset: value.asset.into(),
            amount: value.amount.to_string(),
            payer: value.payer.map(Into::into),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Block {
    pub position: u64,
    pub height: u64,
    pub hash: String,
    pub parent: Option<ParentBlock>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ParentBlock {
    pub position: u64,
    pub hash: String,
}

impl From<base::BlockRef> for Block {
    fn from(value: base::BlockRef) -> Self {
        Self::from(&value)
    }
}

impl From<&base::BlockRef> for Block {
    fn from(value: &base::BlockRef) -> Self {
        Self {
            position: value.position.0,
            height: value.height.0,
            hash: hex::encode(&value.hash.0),
            parent: value.parent.as_ref().map(|parent| ParentBlock {
                position: parent.position.0,
                hash: hex::encode(&parent.hash.0),
            }),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Status {
    Included {
        block: Block,
        confirmations: u64,
    },
    Confirmed {
        block: Block,
        confirmations: u64,
    },
    Failed {
        block: Block,
        reason: Option<String>,
    },
}

impl From<wallets::HistoryStatus> for Status {
    fn from(value: wallets::HistoryStatus) -> Self {
        match value {
            wallets::HistoryStatus::Included {
                block,
                confirmations,
            } => Self::Included {
                block: block.into(),
                confirmations,
            },
            wallets::HistoryStatus::Confirmed {
                block,
                confirmations,
            } => Self::Confirmed {
                block: block.into(),
                confirmations,
            },
            wallets::HistoryStatus::Failed { block, reason } => Self::Failed {
                block: block.into(),
                reason,
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Transaction {
    pub scope: Scope,
    pub transaction_id: String,
    pub status: Status,
    pub movements: Vec<Movement>,
    pub fee: Option<Fee>,
}

impl From<wallets::HistoryEntry> for Transaction {
    fn from(value: wallets::HistoryEntry) -> Self {
        Self {
            scope: value.scope.into(),
            transaction_id: value.transaction_id.value,
            status: value.status.into(),
            movements: value.movements.into_iter().map(Into::into).collect(),
            fee: value.fee.map(Into::into),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct TransactionPage {
    pub checkpoint: Option<Block>,
    pub transactions: Vec<Transaction>,
    pub next_cursor: Option<String>,
}

impl TryFrom<wallets::History> for TransactionPage {
    type Error = Error;

    fn try_from(history: wallets::History) -> Result<Self, Self::Error> {
        let next_cursor = history
            .next
            .as_ref()
            .map(HistoryCursor::encode)
            .transpose()?;
        Ok(Self {
            checkpoint: history.checkpoint.map(Into::into),
            transactions: history.transactions.into_iter().map(Into::into).collect(),
            next_cursor,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct HistoryQuery {
    pub cursor: Option<String>,
    #[serde(default = "HistoryQuery::default_limit")]
    #[param(minimum = 1, maximum = 1000, default = 100)]
    pub limit: usize,
}

impl TryFrom<HistoryQuery> for wallets::HistoryRequest {
    type Error = Error;

    fn try_from(query: HistoryQuery) -> Result<Self, Self::Error> {
        if query.limit == 0 || query.limit > 1_000 {
            return Err(Error::invalid_request(
                "history limit must be between 1 and 1000",
            ));
        }
        let after = query
            .cursor
            .as_deref()
            .map(HistoryCursor::decode)
            .transpose()?;
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
    params(WalletPath, HistoryQuery),
    responses(
        (status = 200, body = TransactionPage),
        (status = 400, body = Body),
        (status = 404, body = Body),
        (status = 409, description = "Cursor checkpoint is no longer canonical",
            body = Body),
        (status = 500, body = Body),
        (status = 503, body = Body)
    ),
    tag = "transactions"
)]
async fn read(
    State(state): State<HttpState>,
    Path(path): Path<WalletPath>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<TransactionPage>, Error> {
    let request = query.try_into()?;
    Ok(Json(
        state.wallets.history(&path.id, request).await?.try_into()?,
    ))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SendFunds {
    pub destination: AddressInput,
    pub amount: String,
}

impl TryFrom<SendFunds> for (wallets::AddressText, base::Decimal) {
    type Error = Error;

    fn try_from(request: SendFunds) -> Result<Self, Self::Error> {
        let amount = request
            .amount
            .parse()
            .map_err(|_| Error::invalid_request("amount must be an exact base-10 decimal"))?;
        Ok((request.destination.into(), amount))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Submission {
    pub transaction_id: String,
}

#[utoipa::path(
    post,
    path = "/v1/wallets/{id}/transactions",
    description = "Submits one transfer through the wallet's configured asset family. The native \
        SOL integration is reserved for this shared route; no Solana-only transaction route is \
        defined. A 202 response means submitted, not confirmed.",
    params(WalletPath),
    request_body = SendFunds,
    responses(
        (status = 202, description = "Transaction submitted", body = Submission),
        (status = 400, body = Body),
        (status = 404, body = Body),
        (status = 422, body = Body),
        (status = 503, description = "Transaction unavailable or exact-envelope submission \
            outcome ambiguous; ambiguous_transaction_id is present only for an unknown outcome",
            body = Body)
    ),
    tag = "transactions"
)]
async fn send(
    State(state): State<HttpState>,
    Path(path): Path<WalletPath>,
    _query: SubmitQuery,
    request: Result<Json<SendFunds>, JsonRejection>,
) -> Result<(StatusCode, Json<Submission>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let (destination, amount) = request.try_into()?;
    let id = state.wallets.send(&path.id, destination, amount).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(Submission {
            transaction_id: id.to_string(),
        }),
    ))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WalletTransfer {
    pub wallet_id: String,
    pub destination: AddressInput,
    pub amount: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TransferRequest {
    /// Transfers for one configured asset family, in execution order.
    #[schema(min_items = 1, max_items = 50)]
    pub transfers: Vec<WalletTransfer>,
}

impl TryFrom<TransferRequest> for Vec<wallets::WalletTransfer<String>> {
    type Error = Error;

    fn try_from(request: TransferRequest) -> Result<Self, Self::Error> {
        if request.transfers.len() > wallets::MAX_TRANSFERS {
            return Err(Error::invalid_request(format!(
                "at most {} transfers are allowed",
                wallets::MAX_TRANSFERS
            )));
        }
        request
            .transfers
            .into_iter()
            .enumerate()
            .map(|(failed_index, transfer)| {
                let amount = transfer.amount.parse().map_err(|_| {
                    Error::invalid_batch(failed_index, "amount must be an exact base-10 decimal")
                })?;
                Ok(wallets::WalletTransfer {
                    wallet: transfer.wallet_id,
                    to: transfer.destination.into(),
                    amount,
                })
            })
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct TransferResponse {
    /// Accepted transaction IDs in chain-native submission order.
    pub transaction_ids: Vec<String>,
}

#[utoipa::path(
    post,
    path = "/v1/transactions",
    description = "Submits one ordered exact-asset batch through the configured wallet family. \
        The native SOL integration is reserved for this shared route; no Solana-only transaction \
        route is defined. Requests mixing wallet families are rejected before any transaction is \
        submitted. Bitcoin may group occurrences into one transaction. Ethereum reserves \
        consecutive nonces per sender and prepares the whole batch before submitting exact \
        envelopes in request order. Definitely acknowledged transaction IDs are returned only \
        when a non-empty prefix was accepted, and a failed index is returned only when one \
        original occurrence truthfully failed.",
    request_body = TransferRequest,
    responses(
        (status = 202, description = "Asset batch submitted", body = TransferResponse),
        (status = 400, body = Body),
        (status = 404, body = Body),
        (status = 422, description = "Definite batch failure; transaction_ids is present only for \
            a definitely acknowledged prefix and failed_index only for a truthful item-scoped \
            failure",
            body = Body),
        (status = 503, description = "Batch unavailable or exact-envelope submission outcome \
            ambiguous; transaction_ids is present only for a definitely acknowledged prefix, \
            failed_index only for a truthful item-scoped failure, and ambiguous_transaction_id \
            only for an unknown outcome",
            body = Body)
    ),
    tag = "transactions"
)]
async fn send_all(
    State(state): State<HttpState>,
    _query: SubmitQuery,
    request: Result<Json<TransferRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<TransferResponse>), Error> {
    let Json(request) = request.map_err(Error::invalid_json)?;
    let transaction_ids = state
        .wallets
        .send_all(request.try_into()?)
        .await?
        .into_iter()
        .map(|id| id.to_string())
        .collect();
    Ok((
        StatusCode::ACCEPTED,
        Json(TransferResponse { transaction_ids }),
    ))
}

#[cfg(test)]
mod tests {
    use base::{BlockHash, BlockHeight, BlockRef, Decimal};
    use indexing::{AssetId, CanonicalAddress, ChainId, IndexScope, MovementId, TransactionRef};

    use super::*;

    fn scope() -> IndexScope {
        IndexScope {
            chain: ChainId("bitcoin".to_owned()),
            network: "regtest".to_owned(),
        }
    }

    fn asset() -> wallets::HistoryAsset {
        wallets::HistoryAsset {
            id: AssetId {
                chain: scope().chain,
                asset: "native".to_owned(),
            },
            name: Some("Bitcoin".to_owned()),
            ticker: Some("BTC".to_owned()),
            decimals: 8,
        }
    }

    fn address(value: &str) -> CanonicalAddress {
        CanonicalAddress {
            scope: scope(),
            value: value.to_owned(),
        }
    }

    #[test]
    fn history_conversion_preserves_typed_facts() {
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
        let history = wallets::History {
            checkpoint: Some(block.clone()),
            transactions: vec![wallets::HistoryEntry {
                scope: scope(),
                transaction_id: TransactionRef {
                    scope: scope(),
                    value: "tx-1".to_owned(),
                },
                status: wallets::HistoryStatus::Confirmed {
                    block,
                    confirmations: 7,
                },
                movements: vec![
                    wallets::HistoryMovement {
                        id: MovementId("input:0".to_owned()),
                        kind: indexing::MovementKind::Input,
                        asset: asset(),
                        amount: "1.23000000".parse::<Decimal>().expect("decimal"),
                        from: Some(address("sender")),
                        to: None,
                    },
                    wallets::HistoryMovement {
                        id: MovementId("output:0".to_owned()),
                        kind: indexing::MovementKind::Output,
                        asset: asset(),
                        amount: "1.22000000".parse::<Decimal>().expect("decimal"),
                        from: None,
                        to: Some(address("recipient")),
                    },
                ],
                fee: Some(wallets::HistoryFee {
                    asset: asset(),
                    amount: "0.01000000".parse::<Decimal>().expect("decimal"),
                    payer: Some(address("sender")),
                }),
            }],
            next: None,
        };

        let page = TransactionPage::try_from(history).expect("typed history");
        let transaction = &page.transactions[0];
        assert_eq!(page.checkpoint.as_ref().map(|value| value.height), Some(12));
        assert_eq!(transaction.scope.network, "regtest");
        assert_eq!(transaction.transaction_id, "tx-1");
        assert_eq!(transaction.movements.len(), 2);
        assert_eq!(transaction.movements[0].amount, "1.23");
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

    #[test]
    fn history_cursor_preserves_checkpoint_and_position() {
        use base64::Engine;

        let checkpoint = BlockRef {
            position: base::BlockPosition(42),
            height: BlockHeight(12),
            hash: BlockHash(vec![0xab; 32]),
            parent: Some(base::BlockParent {
                position: base::BlockPosition(40),
                hash: BlockHash(vec![0xcd; 32]),
            }),
            timestamp: Some(44),
        };
        let cursor = indexing::HistoryCursor {
            checkpoint: Some(checkpoint),
            position: indexing::HistoryPosition {
                height: BlockHeight(9),
                transaction: TransactionRef {
                    scope: scope(),
                    value: "tx-1".to_owned(),
                },
            },
        };

        let encoded = HistoryCursor::encode(&cursor).expect("cursor encodes");
        let decoded = HistoryCursor::decode(&encoded).expect("cursor decodes");

        assert_eq!(decoded, cursor);
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .expect("base64 cursor");
        let wire: serde_json::Value = serde_json::from_slice(&bytes).expect("JSON cursor");
        assert_eq!(
            wire["checkpoint"],
            serde_json::json!({
                "block": {
                    "position": 42,
                    "height": 12,
                    "hash": "ab".repeat(32),
                    "parent": { "position": 40, "hash": "cd".repeat(32) }
                },
                "timestamp": 44
            })
        );
    }

    fn cursor_wire() -> serde_json::Value {
        serde_json::json!({
            "chain": "bitcoin",
            "network": "regtest",
            "transaction": "tx-1",
            "height": 9,
            "checkpoint": {
                "block": {
                    "position": 42,
                    "height": 12,
                    "hash": "abab",
                    "parent": { "position": 40, "hash": "cdcd" }
                },
                "timestamp": 44
            }
        })
    }

    fn encode_cursor_wire(wire: &serde_json::Value) -> String {
        use base64::Engine;

        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(wire).expect("cursor serializes"))
    }

    #[test]
    fn history_cursor_rejects_unknown_fields_at_every_boundary() {
        let valid = cursor_wire();
        assert!(HistoryCursor::decode(&encode_cursor_wire(&valid)).is_ok());

        for pointer in [
            "",
            "/checkpoint",
            "/checkpoint/block",
            "/checkpoint/block/parent",
        ] {
            let mut invalid = valid.clone();
            invalid.pointer_mut(pointer).expect("cursor object")["unexpected"] = true.into();
            assert!(
                HistoryCursor::decode(&encode_cursor_wire(&invalid)).is_err(),
                "unknown fields must be rejected at {pointer}"
            );
        }
    }

    #[test]
    fn history_cursor_rejects_empty_or_malformed_block_hashes() {
        let valid = cursor_wire();
        assert!(HistoryCursor::decode(&encode_cursor_wire(&valid)).is_ok());

        for pointer in ["/checkpoint/block/hash", "/checkpoint/block/parent/hash"] {
            for hash in ["", "a", "zz"] {
                let mut invalid = valid.clone();
                *invalid.pointer_mut(pointer).expect("hash") = hash.into();
                assert!(
                    HistoryCursor::decode(&encode_cursor_wire(&invalid)).is_err(),
                    "invalid hash {hash:?} must be rejected at {pointer}"
                );
            }
        }
    }

    #[test]
    fn history_cursor_preserves_absent_checkpoint_parent_and_timestamp() {
        use base64::Engine;

        for pointer in [
            "/checkpoint",
            "/checkpoint/block/parent",
            "/checkpoint/timestamp",
        ] {
            let mut wire = cursor_wire();
            *wire.pointer_mut(pointer).expect("optional field") = serde_json::Value::Null;
            let encoded = encode_cursor_wire(&wire);
            let decoded = HistoryCursor::decode(&encoded).expect("optional field may be absent");
            let reencoded = HistoryCursor::encode(&decoded).expect("cursor encodes");
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(&reencoded)
                .expect("base64 cursor");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&bytes).expect("JSON cursor"),
                wire
            );
            assert_eq!(
                HistoryCursor::decode(&reencoded).expect("cursor decodes"),
                decoded
            );
        }
    }

    #[test]
    fn history_cursor_rejects_previous_flat_checkpoint() {
        let mut wire = cursor_wire();
        let timestamp = wire["checkpoint"]["timestamp"].take();
        wire["checkpoint"] = wire["checkpoint"]["block"].take();
        wire["checkpoint"]["timestamp"] = timestamp;

        assert!(
            HistoryCursor::decode(&encode_cursor_wire(&wire)).is_err(),
            "previous flat cursors must not decode after the composition cutover"
        );
    }

    #[test]
    fn history_cursor_rejects_height_only_checkpoint() {
        use base64::Engine;

        let old = serde_json::json!({
            "chain": "bitcoin",
            "network": "regtest",
            "transaction": "tx-1",
            "height": 9,
            "checkpoint": {
                "height": 12,
                "hash": "abab",
                "parent_hash": "cdcd",
                "timestamp": 44
            }
        });
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&old).expect("old cursor serializes"));

        assert!(
            HistoryCursor::decode(&encoded).is_err(),
            "height-only cursors must not decode after the coordinate cutover"
        );
    }

    #[test]
    fn history_cursor_rejects_a_partial_parent_reference() {
        use base64::Engine;

        let invalid = serde_json::json!({
            "chain": "bitcoin",
            "network": "regtest",
            "transaction": "tx-1",
            "height": 9,
            "checkpoint": {
                "block": {
                    "position": 42,
                    "height": 12,
                    "hash": "abab",
                    "parent": { "hash": "cdcd" }
                },
                "timestamp": 44
            }
        });
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&invalid).expect("invalid cursor serializes"));

        assert!(
            HistoryCursor::decode(&encoded).is_err(),
            "a parent must contain position and hash atomically"
        );
    }
}
