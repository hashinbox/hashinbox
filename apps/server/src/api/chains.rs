//! HTTP configuration models; chain validation and persistence belong to the runtime.
use super::{
    Application,
    error::{Error, ErrorBody},
};
use crate::settings::{self, Requested};
use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

pub(super) fn routes() -> OpenApiRouter<Application> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(save))
}

#[derive(Serialize, ToSchema)]
struct ChainSettings {
    id: String,
    revision: u64,
    networks: Vec<NetworkSettings>,
}

#[derive(Serialize, ToSchema)]
struct NetworkSettings {
    name: String,
    /// Chain-owned configuration, excluding RPC addresses and credentials.
    parameters: Value,
    nodes: Vec<NodeStatus>,
    indexing: Requested,
    active_revision: Option<u64>,
    checkpoint: Option<u64>,
    pending_block: Option<u64>,
    history_resume: Option<u64>,
    history_pending: usize,
    rebuild: Option<ReindexStatus>,
    error: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct NodeStatus {
    id: String,
    /// Configured URL for operator editing; environment references remain unresolved.
    url: Option<String>,
    /// URL origin for the node card.
    origin: Option<String>,
    active: bool,
    attempts: u64,
    failures: u64,
    last_failure: Option<String>,
}

#[derive(Serialize, ToSchema)]
struct ReindexStatus {
    id: String,
    phase: settings::Phase,
    target: Option<u64>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct ChainUpdate {
    /// Null when creating a chain; otherwise the last read revision.
    revision: Option<u64>,
    networks: Vec<NetworkUpdate>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct NetworkUpdate {
    name: String,
    /// Chain configuration with an explicit asset tag. Existing parameters are immutable.
    parameters: Value,
    nodes: Vec<NodeUpdate>,
    #[serde(default)]
    indexing: Requested,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct NodeUpdate {
    /// Stable identifier; changing URL or order preserves this ID.
    id: String,
    /// URL string or endpoint object (url, headers, bounds). Omit to retain existing credentials.
    endpoint: Option<Value>,
}

#[derive(Serialize, ToSchema)]
struct SavedRevision {
    revision: u64,
}

impl ChainSettings {
    fn new(id: String, row: settings::ChainRecord, runtime: &crate::runtime::Runtime) -> Self {
        let networks = row
            .chain
            .networks
            .into_iter()
            .map(|n| NetworkSettings::new(&id, n, runtime))
            .collect();
        Self {
            id,
            revision: row.revision,
            networks,
        }
    }
}
impl NetworkSettings {
    fn new(chain: &str, saved: settings::Network, runtime: &crate::runtime::Runtime) -> Self {
        let network = base::Network::new(base::ChainName::new(chain), saved.name.clone());
        let active = runtime.active(&network);
        let health = active
            .as_ref()
            .and_then(|a| a.health.borrow().networks().first().cloned());
        let status = health.as_ref().map(|h| &h.status);
        let selected = active.as_ref().map(|a| a.node());
        let nodes = saved
            .nodes
            .iter()
            .map(|node| {
                NodeStatus::new(
                    node,
                    selected.as_deref() == Some(&node.id),
                    runtime.status.node(&network, &node.id),
                )
            })
            .collect();
        Self {
            name: saved.name,
            parameters: saved.parameters,
            nodes,
            indexing: saved.indexing,
            active_revision: active.as_ref().map(|a| a.revision()),
            checkpoint: status
                .and_then(|s| s.checkpoint.as_ref())
                .map(|b| b.position.0),
            pending_block: status
                .and_then(|s| s.pending_tip.as_ref())
                .map(|b| b.position.0),
            history_resume: status.and_then(|s| s.backfill.as_ref()).map(|b| b.resume.0),
            history_pending: status
                .and_then(|s| s.backfill.as_ref())
                .map_or(0, |b| b.pending),
            rebuild: saved.rebuild.map(|r| ReindexStatus {
                id: r.id,
                phase: r.phase,
                target: r.target,
            }),
            error: runtime.status.error(&network).or_else(|| {
                health
                    .and_then(|h| h.failure)
                    .map(|_| "Indexing requires attention; inspect network logs".into())
            }),
        }
    }
}
impl NodeStatus {
    fn new(node: &settings::Node, active: bool, stats: Option<json_rpc::Statistics>) -> Self {
        let stats = stats.map(|s| s.snapshot());
        Self {
            id: node.id.clone(),
            url: node
                .endpoint
                .as_str()
                .or_else(|| node.endpoint.get("url")?.as_str())
                .map(str::to_owned),
            origin: node
                .endpoint
                .as_str()
                .or_else(|| node.endpoint.get("url")?.as_str())
                .and_then(|text| url::Url::parse(text).ok())
                .map(|url| url.origin().ascii_serialization()),
            active,
            attempts: stats.as_ref().map_or(0, |s| s.attempts),
            failures: stats.as_ref().map_or(0, |s| s.failures),
            last_failure: stats.and_then(|s| s.last_failure),
        }
    }
}
impl NetworkUpdate {
    fn resolve(self, previous: Option<&settings::Network>) -> Result<settings::Network, Error> {
        if previous.is_some_and(|n| n.parameters != self.parameters) {
            return Err(Error::invalid_request(
                "existing network and asset parameters are immutable",
            ));
        }
        let nodes = self
            .nodes
            .into_iter()
            .map(|node| node.resolve(previous))
            .collect::<Result<_, _>>()?;
        Ok(settings::Network {
            name: self.name,
            parameters: self.parameters,
            nodes,
            indexing: self.indexing,
            resume: previous.map_or(0, |n| n.resume),
            rebuild: previous.and_then(|n| n.rebuild.clone()),
        })
    }
}
impl NodeUpdate {
    fn resolve(self, previous: Option<&settings::Network>) -> Result<settings::Node, Error> {
        let held = previous.and_then(|n| n.nodes.iter().find(|n| n.id == self.id));
        let endpoint = self
            .endpoint
            .or_else(|| held.map(|n| n.endpoint.clone()))
            .ok_or_else(|| Error::invalid_request("new nodes require an endpoint"))?;
        let endpoint = if let (Some(url), Some(mut object)) = (
            endpoint.as_str(),
            held.and_then(|node| node.endpoint.as_object()).cloned(),
        ) {
            object.insert("url".into(), Value::String(url.into()));
            Value::Object(object)
        } else {
            endpoint
        };
        Ok(settings::Node {
            id: self.id,
            endpoint,
        })
    }
}

#[utoipa::path(get, path = "/v1/chains", operation_id = "list_chain_settings", tag = "chains",
    summary = "Read chain settings and runtime progress",
    description = "Returns saved settings, active revisions and reindex progress. \
        Configured RPC URLs are returned for operator editing; \
        environment references are not resolved and authentication headers are omitted.",
    security(("bearerAuth" = [])), responses(
        (status=200, description="Saved chains", body=Vec<ChainSettings>),
        (status=401, description="Unauthorized", body=ErrorBody),
        (status=503, description="Runtime controls unavailable", body=ErrorBody)
))]
async fn list(State(application): State<Application>) -> Result<Json<Vec<ChainSettings>>, Error> {
    let runtime = application
        .runtime
        .ok_or_else(|| Error::indexed("runtime controls unavailable"))?;
    let rows = runtime
        .settings
        .all()
        .into_iter()
        .map(|(id, row)| ChainSettings::new(id, row, &runtime))
        .collect();
    Ok(Json(rows))
}

#[utoipa::path(put, path = "/v1/chains/{chain}",
    operation_id = "save_chain_settings", tag = "chains",
    params(("chain"=String, Path, description="Registered chain name")),
    request_body=ChainUpdate,
    summary = "Save chain nodes and requested indexing state",
    description = "Persists desired settings before asynchronous verification. \
        Failed candidates keep the active connection. Existing network identities and asset \
        parameters cannot be replaced; networks can be added and nodes edited or reordered.",
    security(("bearerAuth" = [])), responses(
        (status=202, description="Desired settings saved", body=SavedRevision),
        (status=400, description="Invalid settings or conflicting revision", body=ErrorBody),
        (status=401, description="Unauthorized", body=ErrorBody),
        (status=503, description="Runtime controls unavailable", body=ErrorBody)
))]
async fn save(
    State(application): State<Application>,
    Path(id): Path<String>,
    body: Result<Json<ChainUpdate>, JsonRejection>,
) -> Result<(StatusCode, Json<SavedRevision>), Error> {
    let runtime = application
        .runtime
        .ok_or_else(|| Error::indexed("runtime controls unavailable"))?;
    let Json(update) = body.map_err(Error::invalid_json)?;
    let existing = runtime.settings.all().remove(&id);
    if existing.as_ref().map(|r| r.revision) != update.revision {
        return Err(Error::invalid_request(
            "configuration revision conflict; reload settings",
        ));
    }
    let mut networks = Vec::new();
    for input in update.networks {
        let previous = existing
            .as_ref()
            .and_then(|r| r.chain.networks.iter().find(|n| n.name == input.name));
        networks.push(input.resolve(previous)?);
    }
    if existing.as_ref().is_some_and(|row| {
        row.chain
            .networks
            .iter()
            .any(|old| !networks.iter().any(|n| n.name == old.name))
    }) {
        return Err(Error::invalid_request(
            "retain existing networks; pause them or remove their nodes",
        ));
    }
    let revision = runtime
        .settings
        .save(id, update.revision, settings::Chain { networks })
        .await
        .map_err(|_| {
            Error::invalid_request("settings rejected; check configuration and revision")
        })?;
    Ok((StatusCode::ACCEPTED, Json(SavedRevision { revision })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn node_url_edit_preserves_identity_and_untouched_connections() {
        let previous = settings::Network {
            name: "regtest".into(),
            parameters: json!({"network":"regtest","asset":"btc"}),
            nodes: vec![settings::Node {
                id: "primary".into(),
                endpoint: json!({"url":"http://old.example", "headers":{"Authorization":"secret"}}),
            }],
            indexing: Requested::Paused,
            resume: 0,
            rebuild: None,
        };
        let held = NodeUpdate {
            id: "primary".into(),
            endpoint: None,
        }
        .resolve(Some(&previous))
        .unwrap();
        assert_eq!(held.endpoint, previous.nodes[0].endpoint);
        let edited = NodeUpdate {
            id: "primary".into(),
            endpoint: Some(json!("http://new.example")),
        }
        .resolve(Some(&previous))
        .unwrap();
        assert_eq!(edited.id, "primary");
        assert_eq!(edited.endpoint["url"], "http://new.example");
        assert_eq!(edited.endpoint["headers"]["Authorization"], "secret");
    }

    #[test]
    fn node_display_returns_the_configured_url_without_resolving_credentials() {
        let configured = "https://rpc.example:8443/v3/${RPC_KEY}?mode=archive";
        let node = settings::Node {
            id: "primary".into(),
            endpoint: json!({"url": configured, "headers":{"Authorization":"secret"}}),
        };
        let status = NodeStatus::new(&node, false, None);
        assert_eq!(status.origin.as_deref(), Some("https://rpc.example:8443"));
        assert_eq!(status.url.as_deref(), Some(configured));
        assert!(!serde_json::to_string(&status).unwrap().contains("secret"));
    }

    #[tokio::test]
    async fn controls_persist_without_a_connected_node_and_never_return_authentication_headers() {
        let directory = tempfile::tempdir().unwrap();
        let config = json!({
            "bind":"127.0.0.1:8080",
            "storage":{"path": directory.path().join("payment.db")}
        });
        let runtime = crate::runtime::Runtime::new(serde_json::from_value(config.clone()).unwrap())
            .await
            .unwrap();
        assert!(
            list(State(runtime.application()))
                .await
                .unwrap()
                .0
                .is_empty()
        );
        let update = json!({"networks":[{
            "name":"regtest", "parameters":{"network":"regtest","asset":"btc"},
            "nodes":[{"id":"primary", "endpoint":{
                "url":"http://127.0.0.1:1", "headers":{"Authorization":"plain-test-secret"}
            }}]
        }]});
        let (status, saved) = save(
            State(runtime.application()),
            Path("bitcoin".into()),
            Ok(Json(serde_json::from_value(update.clone()).unwrap())),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(saved.0.revision, 0);
        assert!(
            save(
                State(runtime.application()),
                Path("bitcoin".into()),
                Ok(Json(serde_json::from_value(update).unwrap()))
            )
            .await
            .is_err()
        );
        let listed =
            serde_json::to_string(&list(State(runtime.application())).await.unwrap().0).unwrap();
        assert!(!listed.contains("plain-test-secret"));
        assert!(listed.contains("http://127.0.0.1:1"));
        runtime
            .settings
            .command(
                &base::Network::new(base::ChainName::new("bitcoin"), "regtest"),
                settings::Action::Pause,
            )
            .await
            .unwrap();
        drop(runtime);
        let runtime = crate::runtime::Runtime::new(serde_json::from_value(config).unwrap())
            .await
            .unwrap();
        let saved = runtime.settings.all();
        let network = &saved["bitcoin"].chain.networks[0];
        assert!(network.indexing == Requested::Paused);
        assert_eq!(
            network.nodes[0].endpoint["headers"]["Authorization"],
            "plain-test-secret"
        );
        assert_eq!(network.resume, 0);
        runtime
            .settings
            .command(
                &base::Network::new(base::ChainName::new("bitcoin"), "regtest"),
                settings::Action::Resume,
            )
            .await
            .unwrap();
        assert_eq!(
            runtime.settings.all()["bitcoin"].chain.networks[0].resume,
            1
        );
        runtime
            .settings
            .command(
                &base::Network::new(base::ChainName::new("bitcoin"), "regtest"),
                settings::Action::Reindex,
            )
            .await
            .unwrap();
        let saved = runtime.settings.all();
        assert!(
            saved["bitcoin"].chain.networks[0]
                .rebuild
                .as_ref()
                .is_some_and(|r| r.phase == settings::Phase::Requested)
        );
        assert_eq!(saved["bitcoin"].chain.networks[0].resume, 1);
    }
}
