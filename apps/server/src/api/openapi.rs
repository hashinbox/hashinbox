use std::sync::Arc;

use axum::{Extension, Json};
use utoipa::openapi::OpenApi;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{Error, ErrorBody};
use crate::state::AppState;

pub(super) fn routes() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new().routes(routes!(read))
}

#[utoipa::path(
    get,
    path = "/openapi.json",
    operation_id = "read_openapi_document",
    summary = "Read this OpenAPI 3 document",
    description = "The contract every route is generated from; public, like the health routes.",
    responses(
        (status = 200, description = "OpenAPI 3 contract", body = Object),
        (status = 500, description = "The contract could not be encoded", body = ErrorBody)
    ),
    tag = "openapi"
)]
async fn read(
    Extension(contract): Extension<Arc<OpenApi>>,
) -> Result<Json<serde_json::Value>, Error> {
    serde_json::to_value(contract.as_ref())
        .map(Json)
        .map_err(|_| Error::invalid_response("OpenAPI contract could not be encoded"))
}

/// The generated document, as a typed client reads it.
#[cfg(test)]
fn document() -> serde_json::Value {
    serde_json::to_value(super::openapi()).expect("the contract serializes")
}

/// `required` is a promise about what the server actually writes: `schema`
/// requires exactly the properties `value` serializes, so an `Option` that is
/// always emitted stays required and a skipped field stays optional.
#[cfg(test)]
pub(crate) fn emits_required(schema: &str, value: impl serde::Serialize) {
    let document = document();
    let value = serde_json::to_value(value).expect("wire value");
    let emitted: std::collections::BTreeSet<&str> = value
        .as_object()
        .expect("wire object")
        .keys()
        .map(String::as_str)
        .collect();
    let required: std::collections::BTreeSet<&str> =
        document["components"]["schemas"][schema]["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{schema} declares no required properties"))
            .iter()
            .map(|value| value.as_str().expect("property name"))
            .collect();
    assert_eq!(
        required, emitted,
        "{schema} requires exactly the properties it emits"
    );
}

/// Properties a generated typed client depends on, asserted against the
/// document this route serves and the copy committed to `docs/openapi.json`.
#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use crate::app::App;
    use std::{collections::BTreeSet, path::PathBuf};

    use serde_json::{Value, json};

    use super::document;
    use crate::api::ErrorBody;
    use crate::api::openapi;

    fn committed_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/openapi.json")
    }

    const METHODS: [&str; 7] = ["get", "put", "post", "delete", "patch", "head", "options"];

    fn operations(document: &Value) -> Vec<(String, String, Value)> {
        let mut operations = Vec::new();
        for (path, item) in document["paths"].as_object().expect("paths") {
            for (method, operation) in item.as_object().expect("path item") {
                if METHODS.contains(&method.as_str()) {
                    operations.push((path.clone(), method.clone(), operation.clone()));
                }
            }
        }
        operations
    }

    /// The document owns its own identity: utoipa-axum's crate metadata must never
    /// reach a published contract.
    #[test]
    fn the_document_declares_its_own_info_and_servers() {
        let document = document();

        assert_eq!(document["openapi"].as_str(), Some("3.1.0"));
        assert_eq!(document["info"]["title"], "payment-api");
        assert_eq!(document["info"]["version"], env!("CARGO_PKG_VERSION"));
        assert!(
            document["info"].get("license").is_none(),
            "an undeclared license must not be published as an empty object"
        );
        let description = document["info"]["description"]
            .as_str()
            .expect("info description");
        assert!(description.contains("payment HTTP routes"), "{description}");
        assert!(
            document["info"].get("contact").is_none(),
            "the contract must not publish another crate's author"
        );
        assert_eq!(
            document["servers"],
            json!([{"url": "/", "description": "The payment server's own origin"}])
        );
    }

    #[test]
    fn every_protected_operation_declares_the_bearer_security_scheme() {
        let document = document();

        assert_eq!(
            document["components"]["securitySchemes"]["bearerAuth"]["type"],
            "http"
        );
        assert_eq!(
            document["components"]["securitySchemes"]["bearerAuth"]["scheme"],
            "bearer"
        );

        let public = ["/health", "/openapi.json"];
        for (path, method, operation) in operations(&document) {
            let declared = operation["security"] == json!([{"bearerAuth": []}]);
            if public.contains(&path.as_str()) {
                assert!(
                    operation.get("security").is_none(),
                    "{method} {path} is public and declares no scheme"
                );
                assert!(
                    operation["responses"].get("401").is_none(),
                    "{method} {path} is public and answers no 401"
                );
                continue;
            }
            assert!(declared, "{method} {path} must declare bearerAuth");
            assert_eq!(
                operation["responses"]["401"]["content"]["application/json"]["schema"]["$ref"],
                "#/components/schemas/ErrorBody",
                "{method} {path} must document its 401 body"
            );
        }
    }

    #[test]
    fn every_operation_has_a_unique_id_a_summary_and_described_responses() {
        let document = document();
        let mut ids = BTreeSet::new();

        for (path, method, operation) in operations(&document) {
            let id = operation["operationId"]
                .as_str()
                .unwrap_or_else(|| panic!("{method} {path} has no operationId"))
                .to_owned();
            assert!(
                ids.insert(id.clone()),
                "operationId `{id}` is used by more than one operation ({method} {path})"
            );
            assert!(
                operation["summary"]
                    .as_str()
                    .is_some_and(|summary| !summary.trim().is_empty()),
                "{method} {path} has no summary"
            );
            assert!(
                operation["description"]
                    .as_str()
                    .is_some_and(|text| !text.trim().is_empty()),
                "{method} {path} has no description"
            );
            for (status, response) in operation["responses"].as_object().expect("responses") {
                assert!(
                    response["description"]
                        .as_str()
                        .is_some_and(|text| !text.trim().is_empty()),
                    "{method} {path} answers {status} without a description"
                );
            }
        }
        assert_eq!(ids.len(), 14, "every route method has one operation");
    }

    /// The closed state enum publishes one shape per wire discriminator.
    #[test]
    fn the_status_union_preserves_the_three_wire_shapes() {
        let document = document();
        let status = &document["components"]["schemas"]["Status"];
        let members = status["oneOf"].as_array().expect("status members");
        assert_eq!(members.len(), 3);
        for kind in ["pending", "confirmed", "failed"] {
            let member = members
                .iter()
                .find(|member| member["properties"]["kind"]["enum"] == json!([kind]))
                .expect("kind shape");
            let required = member["required"].as_array().expect("required fields");
            for field in [
                "kind",
                "block",
                if kind == "failed" {
                    "reason"
                } else {
                    "confirmations"
                },
            ] {
                assert!(required.contains(&json!(field)), "{kind} requires {field}");
            }
        }
    }

    /// Both reads that can be short say how far behind they are, the same way.
    #[test]
    fn the_balance_and_the_history_page_state_how_far_behind_they_are() {
        let document = document();
        assert!(document["components"]["schemas"]["Completeness"].is_null());
        for schema in ["Balance", "TransactionPage"] {
            let behind = &document["components"]["schemas"][schema]["properties"]["behind"];
            assert_eq!(
                behind["type"],
                json!("integer"),
                "{schema}.behind is a count"
            );
            assert!(
                document["components"]["schemas"][schema]["required"]
                    .as_array()
                    .is_some_and(|required| required.iter().any(|value| value == "behind")),
                "{schema} always states how far behind it is"
            );
        }
    }

    /// A network can be ready and still owe history, so the owed range is its own
    /// object on the status row rather than a reason to answer 503.
    #[test]
    fn the_status_row_publishes_the_range_a_network_still_owes() {
        let document = document();
        let row = &document["components"]["schemas"]["NetworkCheckpoint"];

        assert!(
            row["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|value| value == "backfill")),
            "the owed range is always present, null when nothing is owed"
        );
        for property in ["pending", "resume", "anchor", "failure"] {
            assert!(
                document["components"]["schemas"]["Backfill"]["properties"][property].is_object(),
                "Backfill must publish {property}"
            );
        }
    }

    /// The two `skip_serializing_if` fields are absent, never `null`.
    #[test]
    fn omitted_error_fields_are_not_nullable() {
        let document = document();
        let body = &document["components"]["schemas"]["ErrorBody"];

        assert_eq!(body["required"], json!(["message"]));
        for optional in [
            "failed_index",
            "ambiguous_transaction_id",
            "transaction_ids",
        ] {
            let declared = &body["properties"][optional]["type"];
            assert!(
                declared.is_string(),
                "ErrorBody.{optional} must not be nullable: {declared}"
            );
        }
        assert_eq!(
            serde_json::to_value(ErrorBody {
                message: "failed".to_owned(),
                transaction_ids: Vec::new(),
                failed_index: None,
                ambiguous_transaction_id: None,
            })
            .expect("error body"),
            json!({"message": "failed"})
        );
    }

    /// The committed document is what typed clients are generated from; it is
    /// regenerated with `payment-server --openapi > docs/openapi.json`.
    #[test]
    fn the_committed_document_matches_the_generated_one() {
        let path = committed_path();
        let committed = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "{} is missing ({error}); regenerate it with \
                 `cargo run -p payment-server -- --openapi > docs/openapi.json`",
                path.display()
            )
        });
        let generated = format!(
            "{}\n",
            serde_json::to_string_pretty(&openapi()).expect("pretty contract")
        );

        assert_eq!(
            committed, generated,
            "docs/openapi.json is stale; regenerate it with \
             `cargo run -p payment-server -- --openapi > docs/openapi.json`"
        );
    }

    #[tokio::test]
    async fn openapi_serves_the_merged_contract_without_authentication() {
        let app = App::catching_up().await;
        let response = app.call("GET", "/openapi.json", None, false).await;

        assert_eq!(response.status, StatusCode::OK);
        let document = response.json();
        for path in ["/openapi.json", "/health", "/v1/wallets/{id}"] {
            assert!(document["paths"][path]["get"].is_object(), "{path}");
        }
        assert!(document["components"]["schemas"]["Wallet"].is_object());
        assert!(app.sent().is_empty());
    }
}
