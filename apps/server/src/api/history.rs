use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State, rejection::QueryRejection},
};
use serde::{Deserialize, Serialize};
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{
    Payments,
    asset::Asset,
    contract::{Address, Block, Completeness, Network, WalletPath},
    cursor::HistoryCursor,
    error::{Error, ErrorBody},
};

pub(super) fn routes() -> OpenApiRouter<super::Application> {
    OpenApiRouter::new().routes(routes!(read))
}

/// Kind of a [`Movement`].
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum MovementKind {
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
pub(super) struct Movement {
    /// Chain-derived movement ID.
    pub(super) id: String,
    /// What kind of value change this is.
    pub(super) kind: MovementKind,
    /// The moved asset.
    pub(super) asset: Asset,
    /// Exact decimal in display units.
    pub(super) amount: String,
    /// Sender or spent-output owner, when there is one; always present.
    #[schema(required)]
    pub(super) from: Option<Address>,
    /// Recipient or created-output owner, when there is one; always present.
    #[schema(required)]
    pub(super) to: Option<Address>,
}

impl Movement {
    /// One indexed movement in display units, scaled by the unit its own
    /// asset resolves to in `units`. The index reports exact atomic amounts
    /// and never learns an asset's decimals, so this happens here alone.
    fn new(
        movement: indexing::ValueMovement,
        network: &indexing::Network,
        units: &base::Units,
    ) -> Result<Self, Error> {
        let unit = units
            .of(network, &movement.asset)
            .ok_or_else(Error::undeclared)?;
        Ok(Self {
            id: movement.id.0.clone(),
            kind: movement.kind().into(),
            asset: Asset::new(&movement.asset, unit),
            amount: unit
                .display(&movement.amount)
                .map_err(Error::unscalable)?
                .to_string(),
            from: movement.from().cloned().map(Into::into),
            to: movement.to().cloned().map(Into::into),
        })
    }
}

/// A transaction fee, always in the chain's native asset.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub(super) struct Fee {
    /// The chain's native asset.
    pub(super) asset: Asset,
    /// Exact decimal in display units.
    pub(super) amount: String,
    /// Fee payer, when the chain attributes one; always present.
    #[schema(required)]
    pub(super) payer: Option<Address>,
}

impl Fee {
    /// One indexed fee in the chain's native asset and its display units,
    /// which [`TransactionPage::denominated`] settled for the whole page
    /// before any of it was converted.
    fn new(
        fee: indexing::NetworkFee,
        network: &indexing::Network,
        units: &base::Units,
    ) -> Result<Self, Error> {
        let unit = units
            .of(network, &fee.asset)
            .ok_or_else(Error::undeclared)?;
        Ok(Self {
            asset: Asset::new(&fee.asset, unit),
            amount: unit
                .display(&fee.amount)
                .map_err(Error::unscalable)?
                .to_string(),
            payer: fee.payer.map(Into::into),
        })
    }
}

/// Where a history transaction stands relative to its chain's confirmed line.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Status {
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
pub(super) struct Transaction {
    /// The transaction's chain and network.
    pub(super) network: Network,
    /// Chain-native transaction ID.
    pub(super) transaction_id: String,
    /// Inclusion, confirmation, or failure at a block.
    pub(super) status: Status,
    /// Movements of the wallet's asset.
    pub(super) movements: Vec<Movement>,
    /// The fee, when one is attributed; always present.
    #[schema(required)]
    pub(super) fee: Option<Fee>,
}

impl Transaction {
    /// One indexed transaction in the display units of the assets it moved.
    fn new(observed: indexing::ObservedTransaction, units: &base::Units) -> Result<Self, Error> {
        let transaction = observed.transaction;
        Ok(Self {
            network: transaction.transaction_id.network.clone().into(),
            transaction_id: transaction.transaction_id.id.to_string(),
            status: observed.status.into(),
            movements: transaction
                .movements
                .into_iter()
                .map(|movement| Movement::new(movement, &transaction.transaction_id.network, units))
                .collect::<Result<Vec<_>, _>>()?,
            fee: transaction
                .fee
                .map(|fee| Fee::new(fee, &transaction.transaction_id.network, units))
                .transpose()?,
        })
    }
}

/// One page of `GET /v1/wallets/{id}/transactions`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub(crate) struct TransactionPage {
    /// The confirmed line the page was read at; null before the first sync.
    /// Always present.
    #[schema(required)]
    pub(super) checkpoint: Option<Block>,
    /// At most `limit` confirmed transactions; fewer only on the last page.
    /// Append-only: a transaction already returned on a page is never removed
    /// or reordered, so a `next_cursor` stays valid however long it is held.
    pub(super) transactions: Vec<Transaction>,
    /// Transactions in canonical blocks above the confirmed line, oldest
    /// first. They are not in `transactions` because they can still revert,
    /// which would break that list's ordering guarantee. Populated on the
    /// first page only (`cursor` absent), and bounded by the chain's
    /// confirmation depth.
    pub(super) pending: Vec<Transaction>,
    /// Opaque cursor of the next page; null when no transaction follows.
    /// Always present.
    #[schema(required)]
    pub(super) next_cursor: Option<String>,
    /// How much of the history behind this page the index can prove it holds.
    /// `next_cursor` cannot carry it — that speaks about what follows the page,
    /// and a backfill inserts rows *before* it — so a caller paging to the end
    /// mid-backfill reads `incomplete` rather than a complete export.
    pub(super) completeness: Completeness,
}

impl TransactionPage {
    /// One indexed page of `asset` in `network`, in display units.
    ///
    /// This is the whole projection: the index answers in atomic units for an
    /// address and an asset, and the unit each asset's own chain declared —
    /// which only the edge knows — turns it into what a caller reads.
    ///
    /// # Errors
    ///
    /// A 503 when the page leaves `network`, is denominated in anything the
    /// query did not ask for, or moves an asset no configured chain declared a
    /// unit for; a 500 when its cursor cannot be encoded.
    pub(crate) fn new(
        page: indexing::TransactionPage,
        network: &base::Network,
        asset: &base::AssetId,
        units: &base::Units,
    ) -> Result<Self, Error> {
        Self::owned_by(&page, network)?;
        Self::denominated(&page, asset, &base::AssetId::native(network.chain.clone()))?;
        let next_cursor = page.next.as_ref().map(HistoryCursor::encode).transpose()?;
        let convert = |transactions: Vec<indexing::ObservedTransaction>| {
            transactions
                .into_iter()
                .map(|transaction| Transaction::new(transaction, units))
                .collect::<Result<Vec<_>, Error>>()
        };
        Ok(Self {
            completeness: page.completeness.into(),
            checkpoint: page.checkpoint.map(Into::into),
            transactions: convert(page.transactions)?,
            pending: convert(page.pending)?,
            next_cursor,
        })
    }

    /// Rejects a page that left the queried network before any of it is
    /// converted: an identity of another network on this page would reach a
    /// caller as if it were theirs.
    fn owned_by(page: &indexing::TransactionPage, network: &base::Network) -> Result<(), Error> {
        let cursor = page
            .next
            .as_ref()
            .is_some_and(|next| !next.transaction.belongs_to(network));
        if cursor {
            return Err(Error::indexed(
                "indexed history cursor does not belong to the requested network",
            ));
        }
        let transactions = page.transactions.iter().chain(&page.pending);
        for transaction in transactions {
            if !transaction.transaction.transaction_id.belongs_to(network) {
                return Err(Error::indexed(
                    "indexed transaction does not belong to the requested network",
                ));
            }
            let payer = transaction
                .transaction
                .fee
                .as_ref()
                .and_then(|fee| fee.payer.as_ref());
            if payer.is_some_and(|payer| !payer.belongs_to(network)) {
                return Err(Error::indexed(
                    "indexed fee payer does not belong to the transaction network",
                ));
            }
        }
        Ok(())
    }

    /// Rejects a page denominated in anything the query did not ask for,
    /// before any of it is converted: a store answering a query for one asset
    /// has no business returning another, and a fee is the chain's own
    /// currency on every supported chain — which is why `native` is taken from
    /// the queried network rather than from the page.
    fn denominated(
        page: &indexing::TransactionPage,
        asset: &base::AssetId,
        native: &base::AssetId,
    ) -> Result<(), Error> {
        for transaction in page.transactions.iter().chain(&page.pending) {
            if transaction
                .transaction
                .movements
                .iter()
                .any(|movement| &movement.asset != asset)
            {
                return Err(Error::indexed(
                    "indexed history contains an asset outside the query",
                ));
            }
            let fee = transaction.transaction.fee.as_ref();
            if fee.is_some_and(|fee| &fee.asset != native) {
                return Err(Error::indexed(
                    "indexed fee is not priced in the chain's own currency",
                ));
            }
        }
        Ok(())
    }
}

/// Query of `GET /v1/wallets/{id}/transactions`.
// linter:disable rust/redundant-module-prefix -- A bare `Query` is axum's extractor here.
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
        (status = 200, description = "One history page", body = TransactionPage),
        (status = 400, description = "Malformed query, cursor, or limit", body = ErrorBody),
        (status = 401, description = "Missing or invalid bearer token", body = ErrorBody),
        (status = 404, description = "Wallet is unknown", body = ErrorBody),
        (status = 500, description = "The next cursor could not be encoded", body = ErrorBody),
        (status = 503, description = "The index is unavailable", body = ErrorBody)
    ),
    tag = "transactions"
)]
async fn read(
    State(payments): State<Arc<Payments>>,
    Path(path): Path<WalletPath>,
    query: Result<Query<HistoryQuery>, QueryRejection>,
) -> Result<Json<TransactionPage>, Error> {
    let Query(query) = query.map_err(Error::invalid_query)?;
    let page = query.try_into()?;
    let custody = payments.custody(&path.id).await?;
    let _admission = payments.admission(&custody.watch.address.network)?;
    Ok(Json(payments.history(&custody, page).await?))
}

#[cfg(test)]
mod tests {
    use base::{BlockHash, BlockHeight, BlockRef, Decimal};
    use indexing::{AssetId, ChainName, MovementId, Network, TransactionRef};

    use super::*;

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

    /// The projection is the whole of the edge: the index answers in atomic
    /// units and the asset's decimals — which it never learns — turn them
    /// into the same wire body the three-model path produced.
    #[test]
    fn history_conversion_scales_atomic_amounts_and_preserves_typed_facts() {
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
            transaction_id: TransactionRef {
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

        let page = TransactionPage::new(indexed, &network(), &native, &units())
            .expect("projected history");
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
    #[test]
    fn a_page_outside_the_query_is_refused_before_conversion() {
        let elsewhere = Network {
            chain: ChainName::new("bitcoin"),
            name: "mainnet".to_owned(),
        };
        let native = AssetId::native(network().chain);
        let transaction = |network: &Network, asset: AssetId, fee: Option<AssetId>| {
            indexing::ObservedTransaction {
                transaction: indexing::Transaction {
                    transaction_id: TransactionRef {
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
            TransactionPage::new(refused, &network(), &native, &units()).expect_err(reason);
        }
    }

    /// A token's amounts scale by the token's own decimals and its fees by the
    /// chain's currency, in one page: the two scales are resolved per asset,
    /// by the identity each amount already carries.
    #[test]
    fn a_token_page_scales_each_amount_by_its_own_assets_unit() {
        let indexed = indexing::TransactionPage {
            checkpoint: None,
            pending: Vec::new(),
            transactions: vec![indexing::ObservedTransaction {
                transaction: indexing::Transaction {
                    transaction_id: TransactionRef {
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

        let page = TransactionPage::new(indexed, &network(), &token(), &units())
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

        let encoded = HistoryCursor::encode(&position).expect("cursor encodes");
        let decoded = HistoryCursor::decode(&encoded).expect("cursor decodes");

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
        assert!(HistoryCursor::decode(&encode_cursor_wire(&valid)).is_ok());

        let mut invalid = valid.clone();
        invalid["unexpected"] = true.into();
        assert!(HistoryCursor::decode(&encode_cursor_wire(&invalid)).is_err());
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
            HistoryCursor::decode(&encode_cursor_wire(&wire)).is_err(),
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
            HistoryCursor::decode(&encode_cursor_wire(&wire)).is_err(),
            "a checkpoint-bound cursor must not decode after the confirmed-line cutover"
        );
    }
}
