//! Properties a generated typed client depends on, asserted against the
//! document [`openapi`] builds and the copy committed to `docs/openapi.json`.

#![cfg(test)]

use std::{collections::BTreeSet, path::PathBuf};

use serde_json::{Value, json};

use super::{
    asset::Asset,
    contract::{Address, Block, Completeness, Network},
    error::ErrorBody,
    health::{Backfill, IndexFailure, IndexState, NetworkCheckpoint},
    history::{Fee, Movement, MovementKind, Transaction, TransactionPage},
    openapi,
    wallet::Balance,
};

fn document() -> Value {
    serde_json::to_value(openapi()).expect("the contract serializes")
}

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

    let public = ["/healthz", "/openapi.json"];
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
    assert_eq!(ids.len(), 15, "every route method has one operation");
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

/// `required` is a promise about what the server actually writes. Every value
/// here is serialized and compared with the schema, so an `Option` that is
/// always emitted stays required and a skipped field stays optional.
#[test]
fn required_properties_match_what_serde_emits() {
    let document = document();
    let network = Network {
        chain: "fixture".to_owned(),
        name: "regtest".to_owned(),
    };
    let block = Block {
        position: 7,
        height: 7,
        hash: "ab".repeat(32),
        parent: None,
    };
    let asset = Asset {
        chain: "fixture".to_owned(),
        id: "native".to_owned(),
        name: None,
        ticker: None,
        decimals: 8,
    };
    let address = Address {
        network: network.clone(),
        value: "fixture-1".to_owned(),
    };
    let movement = Movement {
        id: "movement-0".to_owned(),
        kind: MovementKind::Transfer,
        asset: asset.clone(),
        amount: "1.5".to_owned(),
        from: None,
        to: Some(address),
    };
    let fee = Fee {
        asset: asset.clone(),
        amount: "0.1".to_owned(),
        payer: None,
    };

    for (schema, value) in [
        ("Network", serde_json::to_value(&network).expect("network")),
        ("Block", serde_json::to_value(&block).expect("block")),
        ("Asset", serde_json::to_value(&asset).expect("asset")),
        (
            "Movement",
            serde_json::to_value(&movement).expect("movement"),
        ),
        ("Fee", serde_json::to_value(&fee).expect("fee")),
        (
            "Balance",
            serde_json::to_value(Balance {
                amount: "1.5".to_owned(),
                pending_incoming: "0.25".to_owned(),
                pending_outgoing: "0".to_owned(),
                observed_height: None,
                pending_height: None,
                completeness: Completeness::Incomplete,
            })
            .expect("balance"),
        ),
        (
            "TransactionPage",
            serde_json::to_value(TransactionPage {
                checkpoint: None,
                transactions: Vec::new(),
                pending: Vec::new(),
                next_cursor: None,
                completeness: Completeness::Overdrawn,
            })
            .expect("page"),
        ),
        (
            "NetworkCheckpoint",
            serde_json::to_value(NetworkCheckpoint {
                network: network.clone(),
                checkpoint: None,
                pending_block: None,
                state: IndexState::Quarantined,
                failure: Some(IndexFailure {
                    kind: "invalid_block".to_owned(),
                    retryable: false,
                    attempts: 1,
                    since: 1_758_614_400,
                    message: "block contains a duplicate transaction".to_owned(),
                }),
                backfill: None,
            })
            .expect("checkpoint"),
        ),
        (
            "Backfill",
            serde_json::to_value(Backfill {
                pending: 2,
                resume: 842_000,
                anchor: 845_112,
                failure: None,
            })
            .expect("backfill"),
        ),
        (
            "Transaction",
            serde_json::to_value(Transaction {
                network,
                transaction_id: "tx-0".to_owned(),
                status: serde_json::from_value(json!({
                    "kind": "failed",
                    "block": serde_json::to_value(&block).expect("block"),
                    "reason": null,
                }))
                .expect("failed status"),
                movements: vec![movement],
                fee: Some(fee),
            })
            .expect("transaction"),
        ),
    ] {
        let emitted: BTreeSet<&str> = value
            .as_object()
            .expect("wire object")
            .keys()
            .map(String::as_str)
            .collect();
        let required: BTreeSet<&str> = document["components"]["schemas"][schema]["required"]
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
}

/// Both reads that can be short report it with the same vocabulary.
#[test]
fn the_balance_and_the_history_page_state_completeness_the_same_way() {
    let document = document();

    assert_eq!(
        document["components"]["schemas"]["Completeness"]["enum"],
        json!([
            "complete_history",
            "verified_opening_balance",
            "incomplete",
            "overdrawn"
        ]),
    );
    for schema in ["Balance", "TransactionPage"] {
        assert_eq!(
            document["components"]["schemas"][schema]["properties"]["completeness"]["$ref"],
            json!("#/components/schemas/Completeness"),
            "{schema}.completeness references the shared vocabulary"
        );
        assert!(
            document["components"]["schemas"][schema]["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|value| value == "completeness")),
            "{schema} always states how complete it is"
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
