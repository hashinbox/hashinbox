//! One chain's saved settings: its networks, their nodes and requested state.

use crate::{api::AssetTag, config::AnyError, storage::Repository};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Chain {
    pub(crate) networks: Vec<Network>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Network {
    pub(crate) name: String,
    pub(crate) parameters: Value,
    pub(crate) nodes: Vec<Node>,
    #[serde(default)]
    pub(crate) indexing: Requested,
    /// Explicit resume requests; unrelated chain edits must not clear quarantine.
    #[serde(default)]
    pub(crate) resume: u64,
    #[serde(default)]
    pub(crate) rebuild: Option<ReindexRequest>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Node {
    pub(crate) id: String,
    pub(crate) endpoint: Value,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Requested {
    #[default]
    Running,
    Paused,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReindexRequest {
    pub(crate) id: String,
    pub(crate) phase: Phase,
    pub(crate) target: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    Requested,
    Purging,
    Scanning,
}

impl Chain {
    pub(crate) fn seed(mut section: Value) -> Result<Self, AnyError> {
        let endpoints = section
            .pointer_mut("/rpc/endpoints")
            .map(Value::take)
            .unwrap_or(json!([]));
        let nodes = endpoints
            .as_array()
            .ok_or("RPC endpoints must be a list")?
            .iter()
            .enumerate()
            .map(|(i, endpoint)| Node {
                id: format!("node-{}", i + 1),
                endpoint: endpoint.clone(),
            })
            .collect();
        if let Some(rpc) = section.get_mut("rpc").and_then(Value::as_object_mut) {
            rpc.remove("endpoints");
        }
        let name = section
            .get("network")
            .and_then(Value::as_str)
            .ok_or("network name is required")?
            .to_owned();
        Ok(Self {
            networks: vec![Network {
                name,
                parameters: section,
                nodes,
                indexing: Requested::Running,
                resume: 0,
                rebuild: None,
            }],
        })
    }

    pub(crate) fn validate(&self, id: &str) -> Result<(), AnyError> {
        if self.networks.is_empty() || self.networks.len() > 32 {
            return Err("a chain requires between one and 32 networks".into());
        }
        let mut names = BTreeSet::new();
        let mut tags = BTreeSet::new();
        for network in &self.networks {
            if !names.insert(&network.name) {
                return Err("duplicate network".into());
            }
            network.validate(id)?;
            for tag in network.tags()? {
                if !tags.insert(tag) {
                    return Err("duplicate asset tag".into());
                }
            }
        }
        Ok(())
    }
}

impl Network {
    pub(crate) fn tags(&self) -> Result<Vec<String>, AnyError> {
        let main = self
            .parameters
            .get("asset")
            .and_then(Value::as_str)
            .ok_or("an explicit asset tag is required for saved configuration")?;
        let mut tags = vec![main.to_owned()];
        if let Some(tokens) = self.parameters.get("tokens").and_then(Value::as_array) {
            for token in tokens {
                tags.push(
                    token
                        .get("asset")
                        .and_then(Value::as_str)
                        .ok_or("token asset tag required")?
                        .to_owned(),
                );
            }
        }
        Ok(tags)
    }

    pub(crate) fn validate(&self, chain: &str) -> Result<(), AnyError> {
        if self.name.is_empty() || self.name.len() > 64 || self.nodes.len() > 16 {
            return Err("invalid network name or node count".into());
        }
        if self.parameters.pointer("/rpc/endpoints").is_some() {
            return Err("endpoints belong in nodes".into());
        }
        let mut ids = BTreeSet::new();
        for node in &self.nodes {
            if node.id.is_empty()
                || node.id.len() > 64
                || !node
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
                || !ids.insert(&node.id)
            {
                return Err("invalid or duplicate node ID".into());
            }
            node.parsed()?.validate()?;
        }
        let configured = self.chains(chain, self.nodes.first())?;
        let networks = configured.networks();
        if networks.len() != 1 || networks[0].name != self.name {
            return Err("network identity does not match its settings".into());
        }
        self.tags()?;
        Ok(())
    }

    pub(crate) fn chains(
        &self,
        chain: &str,
        node: Option<&Node>,
    ) -> Result<sdk_config::Chains<AssetTag, Repository>, AnyError> {
        let mut section = self.parameters.clone();
        let object = section
            .as_object_mut()
            .ok_or("chain parameters must be an object")?;
        let rpc = object.entry("rpc").or_insert(json!({}));
        rpc.as_object_mut()
            .ok_or("RPC settings must be an object")?
            .insert(
                "endpoints".into(),
                json!([node
                    .map(|n| n.endpoint.clone())
                    .unwrap_or(json!("http://127.0.0.1:1"))]),
            );
        let sections = serde_json::from_value(json!({chain: section}))
            .map_err(|_| "invalid chain parameters")?;
        crate::chains::registry()
            .chains(&sections)
            .map_err(|_| "chain parameters failed validation".into())
    }
}

impl Node {
    pub(crate) fn parsed(&self) -> Result<json_rpc::Endpoint, AnyError> {
        serde_json::from_value(self.endpoint.clone()).map_err(|_| "invalid RPC endpoint".into())
    }
}
