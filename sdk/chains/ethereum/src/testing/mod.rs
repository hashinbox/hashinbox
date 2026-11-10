//! A mock Sepolia for tests: [`Model`] answers every method this crate
//! calls from replies recorded off a real node (`recordings/`, refreshed by
//! [`record`]), and [`Ethereum`] is its `base::testing::TestChain`.

mod model;
mod record;
mod render;

use std::{sync::Arc, time::Duration};

use ::rpc::{BoxFuture, CallError};
use alloy_primitives::{Address, U256};
use base::{
    AssetConfig, AssetKind, NetworkConfig,
    testing::{Caps, Chain, Clock, Node, NodeControl, Recording, TestChain},
};
use serde_json::json;

pub use model::{Model, Outcome, Seal, State, Tx};
pub use record::record;

/// Sepolia's USDC, the token every recording and fixture moves.
pub const TOKEN: &str = "0x1c7d4b196cb0c7b01d743fbc6116a902379c7238";
/// The ERC-20 `Transfer` event topic.
const TRANSFER: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
/// The account every [`TestChain::fund`] pays from.
const FAUCET: Address = Address::repeat_byte(0xfa);
/// Where each block's filler transfer goes (Sepolia blocks are never empty).
const SINK: Address = alloy_primitives::address!("000000000000000000000000000000000000dead");
/// Blocks a node's `finalized` trails its head by.
const FINALITY: u64 = 3;

use crate::fee::PLACEHOLDER;

/// Every recording, by name.
const RECORDINGS: &[(&str, &str)] = &[
    (
        "eth_blockNumber",
        include_str!("recordings/eth_blockNumber.json"),
    ),
    (
        "eth_call.decimals",
        include_str!("recordings/eth_call.decimals.json"),
    ),
    ("eth_chainId", include_str!("recordings/eth_chainId.json")),
    (
        "eth_estimateGas",
        include_str!("recordings/eth_estimateGas.json"),
    ),
    (
        "eth_estimateGas.reverted",
        include_str!("recordings/eth_estimateGas.reverted.json"),
    ),
    (
        "eth_getBlockByNumber.finalized",
        include_str!("recordings/eth_getBlockByNumber.finalized.json"),
    ),
    (
        "eth_getBlockByNumber.full",
        include_str!("recordings/eth_getBlockByNumber.full.json"),
    ),
    (
        "eth_getBlockByNumber.future",
        include_str!("recordings/eth_getBlockByNumber.future.json"),
    ),
    (
        "eth_getBlockByNumber.hashes",
        include_str!("recordings/eth_getBlockByNumber.hashes.json"),
    ),
    (
        "eth_getBlockByNumber.latest",
        include_str!("recordings/eth_getBlockByNumber.latest.json"),
    ),
    (
        "eth_getBlockReceipts",
        include_str!("recordings/eth_getBlockReceipts.json"),
    ),
    (
        "eth_getBlockReceipts.pruned",
        include_str!("recordings/eth_getBlockReceipts.pruned.json"),
    ),
    ("eth_getLogs", include_str!("recordings/eth_getLogs.json")),
    (
        "eth_getTransactionByHash",
        include_str!("recordings/eth_getTransactionByHash.json"),
    ),
    (
        "eth_getTransactionByHash.unknown",
        include_str!("recordings/eth_getTransactionByHash.unknown.json"),
    ),
    (
        "eth_getTransactionCount",
        include_str!("recordings/eth_getTransactionCount.json"),
    ),
    (
        "eth_getTransactionReceipt",
        include_str!("recordings/eth_getTransactionReceipt.json"),
    ),
    (
        "eth_getTransactionReceipt.pruned",
        include_str!("recordings/eth_getTransactionReceipt.pruned.json"),
    ),
    (
        "eth_maxPriorityFeePerGas",
        include_str!("recordings/eth_maxPriorityFeePerGas.json"),
    ),
    (
        "eth_sendRawTransaction.chain",
        include_str!("recordings/eth_sendRawTransaction.chain.json"),
    ),
    (
        "eth_sendRawTransaction.funds",
        include_str!("recordings/eth_sendRawTransaction.funds.json"),
    ),
    (
        "eth_sendRawTransaction.invalid",
        include_str!("recordings/eth_sendRawTransaction.invalid.json"),
    ),
    (
        "eth_sendRawTransaction.underpriced",
        include_str!("recordings/eth_sendRawTransaction.underpriced.json"),
    ),
    (
        "method_not_found",
        include_str!("recordings/method_not_found.json"),
    ),
];

/// Every recording's name.
pub fn recordings() -> impl Iterator<Item = &'static str> {
    RECORDINGS.iter().map(|(name, _)| *name)
}

/// The recording `name`.
///
/// # Panics
///
/// For a name [`recordings`] does not list.
#[must_use]
pub fn recording(name: &str) -> Recording {
    let (_, text) = RECORDINGS
        .iter()
        .find(|(known, _)| *known == name)
        .unwrap_or_else(|| panic!("no recording {name}"));
    Recording::parse(text)
}

/// The mock Sepolia and its nodes. Every mined block carries a filler
/// transfer, as Sepolia's do; a node's `finalized` trails its head by
/// three blocks.
pub struct Ethereum {
    chain: Chain<Model>,
    nodes: Vec<Arc<Node<Model>>>,
}

impl Ethereum {
    /// The chain itself.
    #[must_use]
    pub const fn chain(&self) -> &Chain<Model> {
        &self.chain
    }

    /// Node `index` as itself.
    #[must_use]
    pub fn mock(&self, index: usize) -> &Node<Model> {
        &self.nodes[index]
    }

    fn token() -> Address {
        TOKEN.parse().unwrap_or_default()
    }
}

impl TestChain for Ethereum {
    const CAPS: Caps = Caps::TOKEN
        .with(Caps::REORG)
        .with(Caps::PRUNE)
        .with(Caps::CLUSTER);
    const SUBMIT: &'static str = "eth_sendRawTransaction";

    fn start(nodes: usize) -> Self {
        let model = Model::new(Self::token());
        let mut genesis = State::default();
        let plenty = U256::from(10).pow(U256::from(30));
        genesis.fund(FAUCET, plenty, plenty);
        let block = recording("eth_getBlockByNumber.full");
        let time = block.result()["timestamp"].as_str().unwrap_or("0x0");
        let time = u64::from_str_radix(time.trim_start_matches("0x"), 16).unwrap_or(0);
        let chain = Chain::new(model, genesis, Clock::new(time, 12));
        chain.finality_lag(FINALITY);
        let nodes = (0..nodes).map(|_| Arc::new(chain.node())).collect();
        let ethereum = Self { chain, nodes };
        ethereum.mine(FINALITY + 2);
        ethereum
    }

    fn nodes(&self) -> Vec<Arc<dyn ::rpc::Client>> {
        self.nodes
            .iter()
            .map(|node| Arc::clone(node) as Arc<dyn ::rpc::Client>)
            .collect()
    }

    fn height(node: &Arc<dyn ::rpc::Client>) -> BoxFuture<'_, Result<u64, CallError>> {
        crate::height(node)
    }

    fn network(&self) -> NetworkConfig {
        let native = AssetConfig {
            asset: "native".to_owned(),
            kind: AssetKind::Native,
            contract: None,
            ticker: "ETH".to_owned(),
            decimals: crate::ETH_DECIMALS,
            display_decimals: None,
        };
        let token = AssetConfig {
            asset: TOKEN.to_owned(),
            kind: AssetKind::Contract,
            contract: Some(TOKEN.to_owned()),
            ticker: "USDC".to_owned(),
            decimals: 6,
            display_decimals: None,
        };
        NetworkConfig {
            network: base::Network::new(crate::CHAIN_NAME, "sepolia"),
            chain_id: Some(model::CHAIN_ID.to_string()),
            testnet: true,
            block_time: Duration::from_secs(12),
            confirmations: 2,
            max_history: None,
            explorer_address: None,
            explorer_transaction: None,
            settings: json!({"interval_ms": 5}),
            assets: vec![native, token],
            endpoints: Vec::new(),
        }
    }

    fn fund(&self, to: &str, amount: &str, asset: &str) {
        let to: Address = to.parse().expect("an Ethereum address");
        let amount: U256 = amount.parse().expect("atomic units");
        let tokens = asset != "native";
        let tx = self
            .chain
            .model()
            .faucet(&self.chain.pending(), to, amount, tokens);
        self.chain.submit(tx);
    }

    fn tip(&self) -> u64 {
        self.chain.tip()
    }

    fn mine(&self, n: u64) {
        for _ in 0..n {
            let model = self.chain.model();
            let filler = model.faucet(&self.chain.pending(), SINK, U256::from(1), false);
            self.chain.submit(filler);
            self.chain.mine(1);
        }
    }

    fn hold(&self, on: bool) {
        self.chain.hold(on);
    }

    fn reorg(&self, depth: u64, mined: u64) {
        self.chain.reorg(depth, mined);
    }

    fn prune(&self, below: u64) {
        for node in &self.nodes {
            node.control().prune(below);
        }
    }

    fn node(&self, index: usize) -> NodeControl {
        self.nodes[index].control()
    }
}
