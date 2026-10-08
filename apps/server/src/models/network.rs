//! The chain and network something belongs to, as one nested object.

use serde::{Deserialize, Serialize};

/// The chain and network a wallet, address, or transaction belongs to. It
/// always ships as one nested object, never as two sibling fields.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub(crate) struct Network {
    /// Chain name.
    pub(crate) chain: String,
    /// Configured network name, e.g. `regtest`.
    pub(crate) name: String,
}

impl From<base::Network> for Network {
    fn from(value: base::Network) -> Self {
        Self {
            chain: value.chain.to_string(),
            name: value.name,
        }
    }
}
