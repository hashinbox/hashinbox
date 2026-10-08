//! Node identity and readiness: what [`Client::connect`] verifies.

use base::{BlockHash, BlockHeight};
use serde_json::{Value, json};

use super::{Client, wire::parse_block_hash};
use crate::native::{Error, ErrorKind, Network, json::Object};

/// Supported Bitcoin Core versions: 31.x.
const CORE_VERSIONS: std::ops::Range<u64> = 310_000..320_000;

/// What [`Client::verify`] established about a ready node: its identity and
/// its tip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verification {
    /// Bitcoin Core's numeric version, e.g. `310000`.
    pub version: u64,
    /// The network the node serves.
    pub network: Network,
    /// Its validated tip height.
    pub height: BlockHeight,
    /// Its validated tip.
    pub best_block_hash: BlockHash,
}

impl<C: json_rpc::Client> Client<C> {
    /// Revalidates identity and deployment prerequisites against live node state.
    ///
    /// # Errors
    ///
    /// As for [`Client::connect`].
    pub async fn verify(&self) -> Result<Verification, Error> {
        let info = self.request("getnetworkinfo", json!([])).await?;
        let info = Object::new(&info, "Bitcoin getnetworkinfo result", Error::invalid_reply)?;
        let version = info.u64("version", "Bitcoin Core version")?;
        if !CORE_VERSIONS.contains(&version) {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Bitcoin RPC must be a Bitcoin Core 31.x node",
            ));
        }
        let chain = self.request("getblockchaininfo", json!([])).await?;
        let chain = Object::new(
            &chain,
            "Bitcoin getblockchaininfo result",
            Error::invalid_reply,
        )?;
        let verification = self.verify_chain(&chain, version)?;
        let index = self.request("getindexinfo", json!(["txindex"])).await?;
        verification.require_synced_index(&index)?;
        Ok(verification)
    }

    fn verify_chain(&self, chain: &Object<'_>, version: u64) -> Result<Verification, Error> {
        let name = chain.string("chain", "Bitcoin Core chain")?;
        let network = Network::from_core_chain(name).ok_or_else(|| {
            Error::new(
                ErrorKind::Divergent,
                "Bitcoin Core returned an unsupported chain name",
            )
        })?;
        if network != *self.network() {
            return Err(Error::new(
                ErrorKind::Divergent,
                "Bitcoin RPC network does not match configuration",
            ));
        }
        if chain.bool("pruned", "Bitcoin pruning status")? {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Bitcoin index source requires an unpruned node",
            ));
        }
        let downloading = "Bitcoin initial-block-download status";
        if chain.bool("initialblockdownload", downloading)? {
            return Err(Error::rpc(
                "Bitcoin Core is still in initial block download",
                true,
            ));
        }
        let blocks = chain.u64("blocks", "Bitcoin block height")?;
        if blocks != chain.u64("headers", "Bitcoin header height")? {
            return Err(Error::rpc(
                "Bitcoin Core block and header heights are not synchronized",
                true,
            ));
        }
        let hash = chain.string("bestblockhash", "Bitcoin best block hash")?;
        Ok(Verification {
            version,
            network,
            height: BlockHeight(blocks),
            best_block_hash: parse_block_hash(hash)
                .ok_or_else(|| chain.fail("Bitcoin block hash is invalid"))?,
        })
    }
}

impl Verification {
    fn require_synced_index(&self, info: &Value) -> Result<(), Error> {
        let info = Object::new(info, "Bitcoin getindexinfo result", Error::invalid_reply)?;
        let index = info
            .get("txindex")
            .filter(|index| index.is_object())
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidConfiguration,
                    "Bitcoin Core transaction index is not enabled",
                )
            })?;
        let index = Object::new(index, "Bitcoin transaction index", Error::invalid_reply)?;
        if !index.bool("synced", "Bitcoin transaction-index status")? {
            return Err(Error::rpc(
                "Bitcoin Core transaction index is not synchronized",
                true,
            ));
        }
        let Some(height) = index.get("best_block_height") else {
            return Ok(());
        };
        let height = height.as_u64().ok_or_else(|| {
            index.fail("Bitcoin transaction-index height is not an unsigned integer")
        })?;
        if height != self.height.0 {
            return Err(Error::rpc(
                "Bitcoin Core transaction index has not reached the canonical tip",
                true,
            ));
        }
        Ok(())
    }
}
