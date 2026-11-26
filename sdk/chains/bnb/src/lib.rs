//! BNB Smart Chain for hashinbox: `chain-ethereum`'s EVM machinery (blocks,
//! BNB and BEP-20 legs, EIP-1559 sends, broadcast) under its own chain name,
//! `bnb`, with BNB as its native currency and BSC's cadence as defaults.

use std::sync::Arc;

use anyhow::Context as _;
use base::{NetworkConfig, Service, ServiceFactory, Storage};
pub use chain_ethereum::{Client, Config, Family, Info, Network, Token};
use serde_json::{Value, json};

/// The chain family name: ids read `bnb.mainnet.0x…`.
pub const CHAIN_NAME: base::Chain = base::Chain::from_static("bnb");
/// The native currency's ticker, also its name.
pub const BNB_TICKER: &str = "BNB";
/// BNB Smart Chain: `bnb`, BNB (18 decimals, as ether).
pub const BNB: Family = Family {
    chain: CHAIN_NAME,
    name: BNB_TICKER,
    ticker: BNB_TICKER,
};
/// BSC mainnet's EIP-155 chain ID.
pub const MAINNET_CHAIN_ID: u64 = 56;
/// BSC testnet's EIP-155 chain ID.
pub const TESTNET_CHAIN_ID: u64 = 97;

/// What BSC's ~0.45 s blocks change of Ethereum's defaults, under what a
/// stored network's `settings` say: the indexer wakes every 3 s at the tip,
/// and a send is bumped after 400 blocks (about 3 minutes) unseen, not 50.
fn defaults() -> Value {
    json!({"interval_ms": 3_000, "bump_after": 400})
}

/// A stored BSC network's [`Config`]: `chain-ethereum`'s, of the [`BNB`]
/// family, with BSC's defaults for every knob `settings` leave out.
///
/// # Errors
///
/// Settings that are not an object or do not decode, or a network of
/// another chain.
pub fn config(stored: &NetworkConfig) -> anyhow::Result<Config> {
    let mut settings = defaults();
    let set = stored.settings.as_object().context("BNB settings")?;
    settings
        .as_object_mut()
        .context("BNB defaults")?
        .extend(set.clone());
    let stored = NetworkConfig {
        settings,
        ..stored.clone()
    };
    Config::stored(&stored, BNB)
}

/// Builds a BSC network's service: [`chain_ethereum::Factory`] of a
/// [`config`].
pub struct Factory(pub chain_ethereum::Factory);

impl ServiceFactory for Factory {
    const CHAIN: base::Chain = CHAIN_NAME;

    fn new(stored: &NetworkConfig) -> anyhow::Result<Self> {
        Ok(Self(config(stored)?.into()))
    }

    fn service(
        &self,
        storage: &Storage,
        client: Arc<dyn ::rpc::Client>,
    ) -> anyhow::Result<Service> {
        self.0.service(storage, client)
    }
}
