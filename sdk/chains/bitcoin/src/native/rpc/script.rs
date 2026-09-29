//! Bitcoin's half of the shared scripted node (`json_rpc::testing`): the
//! regtest network its client claims, and the submission method that must
//! never repeat. The fixtures — methods, parameters and payloads — stay in
//! the tests that own them.

pub(crate) use json_rpc::testing::{Reply, Script};

use super::Client;
use crate::native::Network;

/// The method whose envelope must reach exactly one endpoint exactly once.
const SUBMISSION: &str = "sendrawtransaction";

/// What a Bitcoin script adds to the shared double.
pub(crate) trait Scripted: Sized {
    /// A script for a regtest node, with [`SUBMISSION`] pinned to one
    /// execution.
    fn node(replies: impl IntoIterator<Item = Reply>) -> Self;

    /// A client over this script, trusted to serve regtest.
    fn client(&self) -> Client<Script>;
}

impl Scripted for Script {
    fn node(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self::new(replies).once_only(SUBMISSION)
    }

    fn client(&self) -> Client<Script> {
        Client::unverified(self.clone(), Network::Regtest)
    }
}
