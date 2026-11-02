//! Block explorers: per network, the links an address or a transaction
//! opens at, from the configuration's `[ui.explorers.<chain>.<network>]`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One network's explorer links.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Explorer {
    /// An address's page; `{address}` is replaced by the address.
    pub(crate) address: String,
    /// A transaction's page; `{tx}` is replaced by the transaction id.
    pub(crate) transaction: String,
}

/// Every configured explorer, by chain, then network.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(transparent)]
pub(crate) struct Explorers(BTreeMap<String, BTreeMap<String, Explorer>>);

impl Explorers {
    /// The explorer of `network`, when one is configured.
    pub(crate) fn get(&self, network: &base::Network) -> Option<Explorer> {
        self.0
            .get(network.chain.as_str())
            .and_then(|networks| networks.get(&network.name))
            .cloned()
    }
}
