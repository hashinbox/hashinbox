//! The HTTP endpoints, one module per resource, assembled into the router and
//! the OpenAPI document, and the error every endpoint answers with. A shape
//! several endpoints share is a model (`crate::models`); each endpoint's own
//! request and response bodies live in its resource file.

mod assets;
mod balance;
mod health;
pub(crate) mod networks;
mod openapi;
#[cfg(test)]
pub(crate) use openapi::emits_required;
mod transactions;
mod wallets;

use std::sync::Arc;

use axum::{Extension, Router};

use utoipa::{
    Modify, OpenApi,
    openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme},
};
use utoipa_axum::router::OpenApiRouter;

use axum::{
    Json,
    extract::rejection::{JsonRejection, QueryRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::state::AppState;

/// The document root: title, version, description, servers, and the one
/// `bearerAuth` security scheme the protected operations declare. Title and
/// description are stated here and the version is this crate's own, so a
/// published contract never carries a dependency's metadata. The title is the
/// contract's published name, not the name of the crate that serves it.
// linter:disable rust/empty-struct -- Utoipa attaches the document root to a type, not a value.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "payment-api",
        description = "The payment HTTP routes and OpenAPI contract over the chain-neutral \
            wallet and index abstractions. Every asset is an open tag; no chain or asset is \
            named in this contract."
    ),
    servers((url = "/", description = "The payment server's own origin")),
    modifiers(&BearerAuth),
    tags(
        (name = "assets", description = "Configured assets"),
        (name = "wallets", description = "Wallets and balances"),
        (name = "transactions", description = "History, single transfers, and batches"),
        (name = "networks", description = "Configured networks, their RPC nodes, and rescans"),
        (name = "health", description = "Readiness, network state, and RPC statistics"),
        (name = "openapi", description = "This document")
    )
)]
struct Contract;

/// Declares the `bearerAuth` scheme the protected operations reference.
// linter:disable rust/empty-struct -- Utoipa's Modify contract is a trait on a type.
struct BearerAuth;

impl BearerAuth {
    /// The one credential the protected routes accept.
    fn scheme() -> SecurityScheme {
        SecurityScheme::Http(
            HttpBuilder::new()
                .scheme(HttpAuthScheme::Bearer)
                .description(Some(
                    "The configured bearer token; health and this document are public.",
                ))
                .build(),
        )
    }
}

impl Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        // Cargo always defines CARGO_PKG_LICENSE, so Utoipa's env defaults
        // publish an empty license object for a crate that declares none.
        openapi.info.license = None;
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme("bearerAuth", Self::scheme());
    }
}

/// The complete OpenAPI 3 document, without binding a listener.
///
/// `payment-server --openapi` prints exactly this, and `docs/openapi.json`
/// holds the committed copy typed clients are generated from.
#[must_use]
pub(crate) fn openapi() -> utoipa::openapi::OpenApi {
    let (_, mut document) = protected_routes().split_for_parts();
    let (_, public) = public_routes().split_for_parts();
    document.merge(public);
    document
}

/// The complete API router.
///
/// Wallet, transaction and network routes are protected by `config`'s bearer
/// authentication and body limit ([`http_server::protected_router`]); health
/// routes and `GET /openapi.json` are public. The OpenAPI contract is built
/// from the same handlers.
///
/// # Errors
///
/// Returns the [`http_server::ConfigError`] of an invalid `config` (for
/// example a non-loopback bind without TLS termination).
pub(crate) fn router(
    state: AppState,
    config: &http_server::Config,
) -> Result<Router, http_server::ConfigError> {
    let state = Arc::new(state);
    let (protected, mut contract) = protected_routes().split_for_parts();
    let (public, public_contract) = public_routes().split_for_parts();
    contract.merge(public_contract);
    let contract = Arc::new(contract);

    let protected = http_server::protected_router(protected.with_state(state.clone()), config)?;
    Ok(protected
        .merge(public.with_state(state))
        .layer(Extension(contract)))
}

fn protected_routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::with_openapi(Contract::openapi())
        .merge(assets::routes())
        .merge(wallets::routes())
        .merge(balance::routes())
        .merge(transactions::routes())
        .merge(networks::routes())
}

fn public_routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .merge(health::routes())
        .merge(openapi::routes())
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::{Value, json};

    use super::harness::{App, Reply};

    use super::*;

    #[test]
    fn openapi_contains_every_route_method_and_schema() {
        let document = document();

        for (path, method) in [
            ("/health", "get"),
            ("/openapi.json", "get"),
            ("/v1/assets", "get"),
            ("/v1/wallets", "post"),
            ("/v1/wallets", "get"),
            ("/v1/wallets/{id}", "get"),
            ("/v1/wallets/{id}", "delete"),
            ("/v1/wallets/{id}/balance", "get"),
            ("/v1/wallets/{id}/transactions", "get"),
            ("/v1/wallets/{id}/transactions", "post"),
            ("/v1/transactions", "post"),
            ("/v1/networks", "get"),
            ("/v1/networks/{chain}/{network}/rpc", "put"),
            ("/v1/networks/{chain}/{network}/rescans", "post"),
        ] {
            assert_pointer(&document, &format!("/paths/{}/{method}", escape(path)));
        }

        for schema in [
            "Address",
            "Asset",
            "AssetList",
            "Backfill",
            "Balance",
            "Completeness",
            "BatchAccepted",
            "BatchedTransfer",
            "Block",
            "CreateWallet",
            "ErrorBody",
            "Fee",
            "Movement",
            "MovementKind",
            "Network",
            "NetworkCheckpoint",
            "ParentBlock",
            "Queue",
            "Status",
            "Report",
            "Transaction",
            "TransactionPage",
            "Transfer",
            "TransferAccepted",
            "TransferBatch",
            "Wallet",
            "WalletList",
            "NetworkList",
            "ConfiguredNetwork",
            "RpcNode",
            "RpcNodes",
            "RpcEndpoint",
            "ServiceState",
        ] {
            assert_pointer(&document, &format!("/components/schemas/{schema}"));
        }

        for retired in [
            "AddressInput",
            "Destination",
            "Scope",
            "SendFunds",
            "Submission",
            "TransferRequest",
            "TransferResponse",
            "WalletTransfer",
            "NetworkSync",
            "ScopedId",
            "Proof",
            "WatchAddress",
        ] {
            assert!(
                document
                    .pointer(&format!("/components/schemas/{retired}"))
                    .is_none(),
                "the retired schema {retired} must be gone"
            );
        }

        assert_pointer(
            &document,
            "/components/schemas/TransactionPage/properties/transactions/items/$ref",
        );
        assert!(
            document
                .pointer("/components/schemas/TransactionPage/properties/items")
                .is_none(),
            "transaction history must not expose anonymous items"
        );
        assert_pointer(
            &document,
            "/components/schemas/Transaction/properties/movements/items/$ref",
        );
        assert_eq!(
            document.pointer("/components/schemas/Transaction/properties/transaction_id/type"),
            Some(&Value::String("string".to_owned())),
        );
        assert!(
            document["components"]["schemas"]["MovementKind"]["enum"]
                .as_array()
                .is_some_and(|variants| {
                    variants
                        .iter()
                        .all(|value| value.as_str() != Some("internal_transfer"))
                })
        );

        for schema in [
            "CreateWallet",
            "Transfer",
            "BatchedTransfer",
            "TransferBatch",
        ] {
            assert_eq!(
                document.pointer(&format!(
                    "/components/schemas/{schema}/additionalProperties"
                )),
                Some(&Value::Bool(false)),
                "{schema} must reject and document unknown properties"
            );
        }
    }

    /// The `{chain, network}` pair is one nested object on every wire type; it
    /// is never flattened back into two sibling fields.
    #[test]
    fn every_chain_and_network_pair_is_one_nested_network_object() {
        let document = document();

        assert_exact_required_properties(&document, "Network", &["chain", "name"]);
        for schema in ["Wallet", "Address", "Transaction", "NetworkCheckpoint"] {
            assert_eq!(
                document.pointer(&format!(
                    "/components/schemas/{schema}/properties/network/$ref"
                )),
                Some(&serde_json::json!("#/components/schemas/Network")),
                "{schema}.network must reference the shared Network object"
            );
            assert!(
                document
                    .pointer(&format!("/components/schemas/{schema}/properties/chain"))
                    .is_none(),
                "{schema} must not carry a flattened chain field"
            );
            assert!(
                document
                    .pointer(&format!("/components/schemas/{schema}/properties/scope"))
                    .is_none(),
                "{schema} must not carry a scope field"
            );
        }
    }

    #[test]
    fn openapi_names_no_chain_or_closed_asset_list() {
        let document = document();
        let text = document.to_string();

        assert!(document.pointer("/components/schemas/Chain").is_none());
        assert!(
            document
                .pointer("/components/schemas/WalletAsset")
                .is_none()
        );
        let asset = &document["components"]["schemas"]["CreateWallet"]["properties"]["asset"];
        assert_eq!(asset["type"], "string", "CreateWallet.asset is an open tag");
        assert_eq!(asset["pattern"], "^[a-z0-9][a-z0-9-]{0,31}$");
        assert!(
            asset.get("enum").is_none(),
            "CreateWallet.asset lists no assets"
        );
        for chain in ["bitcoin", "ethereum", "solana", "btc", "usdc"] {
            assert!(
                !text
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .any(|word| word.eq_ignore_ascii_case(chain)),
                "the contract must not name `{chain}`"
            );
        }
        assert!(
            document["components"]["schemas"]
                .as_object()
                .is_some_and(|schemas| !schemas.keys().any(|name| name.ends_with("Encoding"))),
            "the address text alone determines its encoding"
        );
        // Word match: "display" (units) must not count as "spl".
        assert!(
            !document
                .to_string()
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|word| word.eq_ignore_ascii_case("spl")),
            "the contract must not publish a token-program asset or route"
        );
    }

    #[test]
    fn openapi_publishes_exact_transaction_contracts() {
        let document = document();
        let transfers = "/components/schemas/TransferBatch/properties/transfers";

        for (schema, properties) in [
            ("Transfer", &["destination", "amount"][..]),
            ("BatchedTransfer", &["wallet_id", "destination", "amount"]),
            ("TransferBatch", &["transfers"]),
            ("TransferAccepted", &["transaction_id"]),
            ("BatchAccepted", &["transaction_ids"]),
        ] {
            assert_exact_required_properties(&document, schema, properties);
        }
        // A destination is address text, never an object: `AddressInput` is gone.
        for pointer in [
            "/components/schemas/Transfer/properties/destination/type",
            "/components/schemas/Transfer/properties/amount/type",
            "/components/schemas/BatchedTransfer/properties/destination/type",
            "/components/schemas/BatchedTransfer/properties/wallet_id/type",
            "/components/schemas/BatchedTransfer/properties/amount/type",
        ] {
            assert_eq!(
                document.pointer(pointer),
                Some(&serde_json::json!("string"))
            );
        }
        for pointer in [
            "/components/schemas/Transfer/properties/destination/$ref",
            "/components/schemas/BatchedTransfer/properties/destination/$ref",
        ] {
            assert!(
                document.pointer(pointer).is_none(),
                "{pointer} must be plain address text"
            );
        }
        assert_eq!(
            document.pointer(&format!("{transfers}/minItems")),
            Some(&serde_json::json!(1)),
        );
        assert_eq!(
            document.pointer(&format!("{transfers}/maxItems")),
            Some(&serde_json::json!(::wallets::MAX_TRANSFERS)),
        );
        assert_eq!(
            document.pointer(&format!("{transfers}/items/$ref")),
            Some(&Value::String(
                "#/components/schemas/BatchedTransfer".to_owned()
            )),
        );
        for unsupported_keyword in ["uniqueItems", "default"] {
            assert!(
                document
                    .pointer(&format!("{transfers}/{unsupported_keyword}"))
                    .is_none(),
                "ordered batch occurrences must not publish {unsupported_keyword}"
            );
        }

        assert_eq!(
            document.pointer("/components/schemas/ErrorBody/required"),
            Some(&serde_json::json!(["message"])),
        );
        assert_description_contains(
            &document,
            "/components/schemas/ErrorBody/properties/transaction_ids/description",
            &["Definitely acknowledged", "Present only"],
        );
        assert_description_contains(
            &document,
            "/components/schemas/ErrorBody/properties/failed_index/description",
            &["original request index", "Present only"],
        );
        assert_description_contains(
            &document,
            "/components/schemas/ErrorBody/properties/ambiguous_transaction_id/description",
            &["exact locally signed envelope", "Present only", "503"],
        );

        for operation in [
            "/paths/~1v1~1wallets~1{id}~1transactions/post/description",
            "/paths/~1v1~1transactions/post/description",
        ] {
            assert_description_contains(&document, operation, &["shared route"]);
        }
        assert_description_contains(
            &document,
            "/paths/~1v1~1transactions/post/description",
            &["Definitely acknowledged", "only when"],
        );
        assert!(
            document["paths"]
                .as_object()
                .is_some_and(|paths| paths.keys().all(|path| !path.contains("solana"))),
            "every asset must use the shared transaction routes"
        );
        assert_eq!(
            document.pointer(
                "/paths/~1v1~1wallets~1{id}~1transactions/post/requestBody/content/\
            application~1json/schema/$ref"
            ),
            Some(&serde_json::json!("#/components/schemas/Transfer")),
        );
        assert_eq!(
            document.pointer(
                "/paths/~1v1~1transactions/post/requestBody/content/\
            application~1json/schema/$ref"
            ),
            Some(&serde_json::json!("#/components/schemas/TransferBatch")),
        );
    }

    fn document() -> Value {
        serde_json::to_value(openapi()).expect("OpenAPI contract must serialize")
    }

    fn escape(path: &str) -> String {
        path.replace('~', "~0").replace('/', "~1")
    }

    fn assert_pointer(document: &Value, pointer: &str) {
        assert!(
            document.pointer(pointer).is_some(),
            "OpenAPI contract is missing {pointer}"
        );
    }

    fn assert_description_contains(document: &Value, pointer: &str, expected: &[&str]) {
        let description = document
            .pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("OpenAPI contract is missing description {pointer}"));
        for fragment in expected {
            assert!(
                description.contains(fragment),
                "OpenAPI description {pointer} is missing `{fragment}`: {description}"
            );
        }
    }

    fn assert_exact_required_properties(document: &Value, schema: &str, expected: &[&str]) {
        let schema_pointer = format!("/components/schemas/{schema}");
        let properties = document
            .pointer(&format!("{schema_pointer}/properties"))
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("OpenAPI schema {schema} is missing properties"));
        assert_eq!(
            properties.len(),
            expected.len(),
            "OpenAPI schema {schema} has unexpected properties: {properties:?}"
        );
        for property in expected {
            assert!(
                properties.contains_key(*property),
                "OpenAPI schema {schema} is missing property {property}"
            );
        }

        let required = document
            .pointer(&format!("{schema_pointer}/required"))
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("OpenAPI schema {schema} is missing required properties"));
        assert_eq!(required.len(), expected.len());
        for property in expected {
            assert!(
                required
                    .iter()
                    .any(|value| value.as_str() == Some(property)),
                "OpenAPI schema {schema} does not require {property}"
            );
        }
    }

    /// Every rejection is the documented JSON error contract, including the ones
    /// Axum and the authentication layer produce before a handler runs. A
    /// `text/plain` rejection would break every generated client.
    #[tokio::test]
    async fn every_rejection_answers_the_json_error_contract() {
        let app = App::ready().await;
        let id = app.wallet_id().await;
        let history = format!("/v1/wallets/{id}/transactions");

        // 1. An unknown query field on the history route.
        let unknown_field = app
            .call(
                "GET",
                &format!("{history}?commitment=finalized"),
                None,
                true,
            )
            .await;
        assert_json_error(&unknown_field, StatusCode::BAD_REQUEST);

        // 2. A limit that is not a number at all.
        let bad_limit = app
            .call("GET", &format!("{history}?limit=all"), None, true)
            .await;
        assert_json_error(&bad_limit, StatusCode::BAD_REQUEST);
        // A numeric limit outside the documented range keeps its own message.
        let out_of_range = app
            .call("GET", &format!("{history}?limit=0"), None, true)
            .await;
        assert_eq!(
            out_of_range.json(),
            json!({"message": "history limit must be between 1 and 1000"})
        );

        // 3. An asset tag that is not a tag, in the path.
        let bad_tag = app
            .call(
                "POST",
                "/v1/wallets",
                Some(json!({"asset": "NOT-A-TAG"})),
                true,
            )
            .await;
        assert_json_error(&bad_tag, StatusCode::BAD_REQUEST);

        // 4. A missing and a wrong bearer token.
        for authorized in [false, true] {
            let response = app
                .raw(
                    "GET",
                    "/v1/wallets",
                    None,
                    false,
                    if authorized {
                        &[("authorization", "Bearer wrong-secret")]
                    } else {
                        &[]
                    },
                )
                .await;
            assert_json_error(&response, StatusCode::UNAUTHORIZED);
        }

        // 5. A body sent without the JSON content type.
        let wrong_content_type = app
            .raw(
                "POST",
                "/v1/wallets",
                Some(r#"{"asset":"btc"}"#),
                true,
                &[("content-type", "text/plain")],
            )
            .await;
        assert_json_error(&wrong_content_type, StatusCode::BAD_REQUEST);
    }

    /// Every error body is `application/json` and carries a non-empty `message`.
    fn assert_json_error(response: &Reply, status: StatusCode) {
        assert_eq!(response.status, status);
        assert_eq!(
            response
                .content_type
                .as_deref()
                .map(|value| value.split(';').next().unwrap_or_default().trim())
                .unwrap_or_default(),
            "application/json",
            "a {status} rejection must answer JSON, not {:?}",
            response.content_type
        );
        let body = response.json();
        assert!(
            body["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()),
            "a {status} rejection must carry a message: {body}"
        );
        assert!(
            body.as_object().is_some_and(|body| {
                body.keys().all(|key| {
                    [
                        "message",
                        "transaction_ids",
                        "failed_index",
                        "ambiguous_transaction_id",
                    ]
                    .contains(&key.as_str())
                })
            }),
            "a {status} rejection must use the ErrorBody schema: {body}"
        );
    }

    #[tokio::test]
    async fn retired_routes_are_refused() {
        let app = App::ready().await;
        for (method, path) in [
            ("GET", "/v1/status"),
            ("POST", "/v1/status/resume"),
            ("POST", "/v1/status/backfill/cancel"),
            ("POST", "/v1/wallets/import"),
            ("GET", "/v1/assets/btc/address?text=addr-a"),
            ("GET", "/healthz"),
            ("POST", "/v1/addresses"),
        ] {
            assert_eq!(
                app.call(method, path, None, true).await.status,
                if path == "/v1/wallets/import" {
                    StatusCode::METHOD_NOT_ALLOWED
                } else {
                    StatusCode::NOT_FOUND
                },
                "{path}"
            );
        }
    }
}

mod harness;

/// The body of every error response: `message` is always present, the
/// submission fields only when they are truthfully known.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, ToSchema)]
pub(crate) struct ErrorBody {
    /// What went wrong.
    pub(crate) message: String,
    /// Definitely acknowledged transaction IDs before an ordered batch failed.
    /// Present only when that accepted prefix is non-empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(required = false)]
    pub(crate) transaction_ids: Vec<String>,
    /// Zero-based original request index. Present only when one public batch
    /// occurrence truthfully failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub(crate) failed_index: Option<usize>,
    /// Canonical ID derived from the exact locally signed envelope. Present only
    /// when its submission outcome remains unknown; its presence produces 503.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    pub(crate) ambiguous_transaction_id: Option<String>,
}

/// One HTTP status the API answers with; every handler failure picks one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    BadRequest,
    NotFound,
    Conflict,
    Unprocessable,
    Internal,
    Unavailable,
}

impl Failure {
    const fn status(self) -> StatusCode {
        match self {
            Self::BadRequest => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Conflict => StatusCode::CONFLICT,
            Self::Unprocessable => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Error {
    failure: Failure,
    message: String,
    transaction_ids: Vec<String>,
    failed_index: Option<usize>,
    ambiguous_transaction_id: Option<String>,
}

impl Error {
    pub(crate) fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(Failure::BadRequest, message)
    }

    /// An index page that breaks the read contract: a 503.
    pub(crate) fn indexed(message: impl Into<String>) -> Self {
        Self::new(Failure::Unavailable, message)
    }

    /// An indexed amount that is not a non-negative whole number of atomic
    /// units, which no chain reports.
    pub(crate) fn unscalable(error: base::DecimalError) -> Self {
        Self::indexed(format!(
            "indexed amount is not a non-negative integer: {error}"
        ))
    }

    /// An indexed asset whose unit no configured chain declared. Composition
    /// refuses to start a process whose factories leave one undeclared, so
    /// this is an index reporting an asset nothing was configured for.
    pub(crate) fn undeclared() -> Self {
        Self::indexed("indexed asset has no unit declared by any configured chain")
    }

    pub(crate) fn invalid_json(_error: JsonRejection) -> Self {
        Self::invalid_request("request body must match the documented JSON schema")
    }

    /// An unparsable query string: the contract answers JSON, never Axum's
    /// plain-text rejection.
    pub(crate) fn invalid_query(_error: QueryRejection) -> Self {
        Self::invalid_request("query parameters must match the documented schema")
    }

    pub(crate) fn invalid_batch(failed_index: usize, message: impl Into<String>) -> Self {
        Self {
            failed_index: Some(failed_index),
            ..Self::invalid_request(message)
        }
    }

    pub(crate) fn invalid_response(message: impl Into<String>) -> Self {
        Self::new(Failure::Internal, message)
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(Failure::NotFound, message)
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::new(Failure::Conflict, message)
    }

    /// A network a rescan cannot touch: nothing of it was composed.
    pub(crate) fn not_in_service() -> Self {
        Self::conflict("network is not in service")
    }

    pub(crate) fn unprocessable(message: impl Into<String>) -> Self {
        Self::new(Failure::Unprocessable, message)
    }

    /// A wallet error of an operation selected by asset tag: an unregistered
    /// tag (`Unsupported`) is `404 wallet asset is not configured`.
    pub(crate) fn by_asset(error: ::wallets::WalletError) -> Self {
        if error.kind == ::wallets::WalletErrorKind::Unsupported {
            return Self::not_found("wallet asset is not configured");
        }
        error.into()
    }

    /// The network is being rescanned: no watch is registered and no send
    /// starts on it until indexing resumes.
    pub(crate) fn rescanning() -> Self {
        Self::conflict("the network is being rescanned; retry once it is indexing again")
    }

    /// The same failure, attributed to one authored batch occurrence.
    #[must_use]
    pub(crate) fn at(self, failed_index: usize) -> Self {
        Self {
            failed_index: Some(failed_index),
            ..self
        }
    }

    fn new(failure: Failure, message: impl Into<String>) -> Self {
        Self {
            failure,
            message: message.into(),
            transaction_ids: Vec::new(),
            failed_index: None,
            ambiguous_transaction_id: None,
        }
    }
}

impl From<::wallets::WalletError> for Error {
    fn from(error: ::wallets::WalletError) -> Self {
        let ::wallets::WalletError {
            kind,
            message,
            ambiguous_transaction_id,
        } = error;
        let failure = match kind {
            ::wallets::WalletErrorKind::Unsupported
            | ::wallets::WalletErrorKind::InvalidSecret
            | ::wallets::WalletErrorKind::InvalidAddress
            | ::wallets::WalletErrorKind::InvalidAmount
            | ::wallets::WalletErrorKind::InvalidBatch
            | ::wallets::WalletErrorKind::AddressMismatch => Failure::BadRequest,
            ::wallets::WalletErrorKind::Duplicate | ::wallets::WalletErrorKind::Conflict => {
                Failure::Conflict
            }
            ::wallets::WalletErrorKind::NotFound => Failure::NotFound,
            ::wallets::WalletErrorKind::Transaction | ::wallets::WalletErrorKind::WatchOnly => {
                Failure::Unprocessable
            }
            ::wallets::WalletErrorKind::Unavailable
            | ::wallets::WalletErrorKind::SourceBusy
            | ::wallets::WalletErrorKind::Generation => Failure::Unavailable,
            // This server pauses a network only to rescan it.
            ::wallets::WalletErrorKind::Paused => return Self::rescanning(),
        };
        let mut response = Self::new(failure, message);
        response.ambiguous_transaction_id = ambiguous_transaction_id.map(|id| id.to_string());
        response
    }
}

/// The index's own failures, with the same status mapping the wallet errors
/// they used to travel through had.
impl From<indexing::IndexError> for Error {
    fn from(error: indexing::IndexError) -> Self {
        let failure = match error.kind {
            indexing::IndexErrorKind::InvalidRequest => Failure::BadRequest,
            indexing::IndexErrorKind::NetworkMismatch => Failure::NotFound,
            indexing::IndexErrorKind::Conflict => Failure::Conflict,
            _ => Failure::Unavailable,
        };
        Self::new(failure, error.message)
    }
}

impl From<::wallets::SendError> for Error {
    fn from(error: ::wallets::SendError) -> Self {
        let mut response = Self::from(error.source);
        response.transaction_ids = error
            .accepted
            .into_iter()
            .map(|id| id.to_string())
            .collect();
        response.failed_index = error.failed_index;
        response.ambiguous_transaction_id = error.ambiguous_transaction_id.map(|id| id.to_string());
        response
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = if self.ambiguous_transaction_id.is_some() {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            self.failure.status()
        };
        (
            status,
            Json(ErrorBody {
                message: self.message,
                transaction_ids: self.transaction_ids,
                failed_index: self.failed_index,
                ambiguous_transaction_id: self.ambiguous_transaction_id,
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod errors {
    use axum::{http::StatusCode, response::IntoResponse};
    use base::TransactionId;
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use wallets::{SendError, WalletError, WalletErrorKind};

    use super::Error;

    fn ambiguous_error(message: &str, id: &str) -> WalletError {
        WalletError {
            kind: WalletErrorKind::Transaction,
            message: message.to_owned(),
            ambiguous_transaction_id: Some(TransactionId::new(id)),
        }
    }

    async fn projection(error: Error) -> (StatusCode, Value) {
        let response = error.into_response();
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("error response body")
            .to_bytes();
        let body = serde_json::from_slice(&bytes).expect("error response JSON");
        (status, body)
    }

    #[tokio::test]
    async fn ambiguous_failures_preserve_reconciliation_metadata() {
        let (status, body) =
            projection(ambiguous_error("single ambiguous", "single-local").into()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body,
            json!({
                "message": "single ambiguous",
                "ambiguous_transaction_id": "single-local"
            })
        );

        let item = SendError::item(
            1,
            vec![TransactionId::new("accepted-0")],
            ambiguous_error("item ambiguous", "item-local"),
        );
        let (status, body) = projection(item.into()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body,
            json!({
                "message": "item ambiguous",
                "transaction_ids": ["accepted-0"],
                "failed_index": 1,
                "ambiguous_transaction_id": "item-local"
            })
        );

        let grouped = SendError::grouped(
            vec![TransactionId::new("accepted-group")],
            ambiguous_error("grouped ambiguous", "grouped-local"),
        );
        let (status, body) = projection(grouped.into()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body,
            json!({
                "message": "grouped ambiguous",
                "transaction_ids": ["accepted-group"],
                "ambiguous_transaction_id": "grouped-local"
            })
        );
    }

    #[tokio::test]
    async fn definite_operation_failures_omit_submission_metadata() {
        for (error, status, body) in [
            (
                WalletError::new(WalletErrorKind::Transaction, "definite single").into(),
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({"message": "definite single"}),
            ),
            (
                SendError::definite(WalletErrorKind::InvalidBatch, "collection failure").into(),
                StatusCode::BAD_REQUEST,
                json!({"message": "collection failure"}),
            ),
            (
                SendError::definite(WalletErrorKind::Unavailable, "operation failure").into(),
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"message": "operation failure"}),
            ),
            (
                SendError::definite(WalletErrorKind::SourceBusy, "single source is busy").into(),
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"message": "single source is busy"}),
            ),
        ] {
            assert_eq!(projection(error).await, (status, body));
        }
    }

    #[tokio::test]
    async fn definite_batch_failures_preserve_only_known_submission_metadata() {
        for (error, status, body) in [
            (
                SendError::item(
                    3,
                    Vec::new(),
                    WalletError::new(WalletErrorKind::SourceBusy, "source is busy"),
                )
                .into(),
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"message": "source is busy", "failed_index": 3}),
            ),
            (
                SendError::item(
                    2,
                    Vec::new(),
                    WalletError::new(WalletErrorKind::Transaction, "item failure"),
                )
                .into(),
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({"message": "item failure", "failed_index": 2}),
            ),
            (
                SendError::grouped(
                    vec![TransactionId::new("accepted-definite")],
                    WalletError::new(WalletErrorKind::Transaction, "grouped failure"),
                )
                .into(),
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({
                    "message": "grouped failure",
                    "transaction_ids": ["accepted-definite"]
                }),
            ),
        ] {
            assert_eq!(projection(error).await, (status, body));
        }
    }

    #[test]
    fn every_failure_maps_to_its_own_status() {
        use super::Failure;

        let statuses = [
            Failure::BadRequest,
            Failure::NotFound,
            Failure::Conflict,
            Failure::Unprocessable,
            Failure::Internal,
            Failure::Unavailable,
        ]
        .map(Failure::status);
        let mut unique = statuses.to_vec();
        unique.sort_unstable_by_key(StatusCode::as_u16);
        unique.dedup();
        assert_eq!(unique.len(), statuses.len(), "one variant per status");
        assert_eq!(statuses[0], StatusCode::BAD_REQUEST);
        assert_eq!(statuses[4], StatusCode::INTERNAL_SERVER_ERROR);
    }
}
