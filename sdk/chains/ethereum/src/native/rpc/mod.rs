//! Ethereum JSON-RPC: the only code that talks to a node.
//!
//! [`Client`] is generic over the `json_rpc::Client` transport, so the
//! composition root supplies a composed `json_rpc` client and tests supply a
//! scripted double. Capabilities that higher layers
//! hold as trait objects are [`Accounts`] (balances, nonces) and [`Submitter`]
//! (broadcast); block reads for indexing are inherent methods in `blocks`.

mod accounts;
mod blocks;
mod call;
mod client;
#[cfg(test)]
pub(crate) mod script;
mod submitter;
mod wire;
mod witness;

pub use accounts::{Accounts, View};
pub use client::Client;
pub use submitter::Submitter;
pub use witness::{Attestation, Attester};

pub(crate) use call::Remote;
pub(crate) use wire::{parse_fixed_data, parse_quantity_u64};

#[cfg(test)]
mod tests {
    use futures_executor::block_on;
    use serde_json::json;

    use super::script::{Reply, Script, Scripted};
    use super::*;
    use crate::native::{ErrorKind, Network};

    #[test]
    fn connect_verifies_the_chain_id_and_nothing_else() {
        let script = Script::node(vec![
            Reply::ok("eth_chainId", json!("0x7a69")).expecting(json!([])),
            Reply::ok("eth_chainId", json!("0x1")),
        ]);
        let network = Network::new(31_337, "anvil");
        let client = block_on(Client::connect(script.clone(), &network)).unwrap();
        assert_eq!(client.network(), &network);
        let error = block_on(Client::connect(script.clone(), &network))
            .err()
            .expect("another chain must fail closed");
        assert_eq!(error.kind, ErrorKind::Divergent);
        assert!(!error.retryable);
        assert_eq!(
            error.message,
            "Ethereum RPC chain ID 1 does not match configured chain ID 31337"
        );
        let error = block_on(Client::connect(script.clone(), &Network::new(0, "anvil")))
            .err()
            .expect("an invalid network must fail before RPC");
        assert_eq!(error.kind, ErrorKind::InvalidConfiguration);
        assert_eq!(script.methods(), ["eth_chainId", "eth_chainId"]);
    }
}
