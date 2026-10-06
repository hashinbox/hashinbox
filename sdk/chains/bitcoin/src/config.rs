//! What this chain needs to run, and how it assembles itself from it.
//!
//! The three steps are the same on every chain: a [`Config`] value — built in
//! code or deserialized from any `serde` format — [`Config::connect`]s to a
//! node and verifies its identity, and the verified [`ChainHandle`] then
//! [`ChainHandle::compose`]s over an opened repository into a
//! [`sdk_config::ComposedChain`]. No storage is opened on the first step, which is
//! what lets an application check every chain's identity before it opens
//! anything. [`sdk_config::Composition`] is what
//! holds this chain beside the others.

use std::{str::FromStr, sync::Arc};

use base::{Networked as _, Unit, Units};
use indexing::Checkpoint as _;
use sdk_config::{ComposedChain, Connection};
use serde::Deserialize;
use wallets::SubmissionRegistrar;

/// What a store must answer for **this** chain to compose over it.
///
/// [`indexing::Repository`] is the minimum any index needs — registration and
/// committing — and the rest is what Bitcoin's own consumers read: the balance
/// fold, the repository's own history query ([`indexing::ConfirmedHistory`])
/// that this chain joins to its overlay for the page `apps/server` projects, and
/// the live outputs coin selection spends. Trait bounds, never a backend: this
/// file names no store implementation. Named here rather than in `wallets`,
/// because it is this
/// chain's requirement and not a rule about stores: an account chain needs no
/// [`indexing::OutputStore`] at all, and a third party's sink that answers none
/// of these still indexes.
pub trait Store:
    indexing::Repository
    + indexing::BalanceStore
    + indexing::OutputStore
    + indexing::ConfirmedHistory
    + Clone
    + Send
    + Sync
    + 'static
{
}

impl<R> Store for R where
    R: indexing::Repository
        + indexing::BalanceStore
        + indexing::OutputStore
        + indexing::ConfirmedHistory
        + Clone
        + Send
        + Sync
        + 'static
{
}
/// The shell every chain's configuration shares, from `sdk/config`: the nodes
/// and their repetition, the cadence this chain indexes at, and the
/// registration hook that lets a document hand this file its own section.
/// `sdk/config` names no chain type, so this file adds Bitcoin's own keys
/// around the shell and implements [`sdk_config::Chain`] for the result.
use sdk_config::{Cadence, Rpc, Transport};

use crate::{
    IndexerParams, Network, RpcClient, WalletConfig, WalletFactory,
    native::{AddressType, AssetKind, BTC, Error, ErrorKind, transaction::FeeRate},
};

/// Everything one Bitcoin network needs. Fields are public; [`Config::new`]
/// fills the chain defaults, and `serde` fills an absent key by calling the
/// same functions, so a value built in code and a value read from a document
/// cannot drift.
///
/// `A` is the asset tag the application keys its wallet factories by; the
/// chain never interprets it.
#[derive(Clone, Deserialize)]
#[serde(
    deny_unknown_fields,
    bound = "A: serde::de::DeserializeOwned + FromStr + Ord"
)]
pub struct Config<A> {
    /// The network this deployment indexes.
    pub network: Network,
    /// The node, or the ordered failover list, and how much a call repeats.
    pub rpc: Rpc,
    /// The asset tag of this network's BTC wallets.
    #[serde(default = "Config::<A>::default_asset")]
    pub asset: A,
    /// Which addresses this deployment's wallets derive. The default is
    /// P2WPKH; `taproot` derives P2TR key-path addresses instead.
    #[serde(default)]
    pub address_type: AddressType,
    /// The ceilings this deployment prices and builds transactions under.
    #[serde(default)]
    pub limits: Limits,
    /// How many blocks below the node's tip this deployment calls
    /// irreversible.
    ///
    /// Bitcoin has no finality, so this number *is* the safety policy, and it
    /// is a trade. A smaller depth makes history and spendable balance appear
    /// sooner; a reorg deeper than it quarantines this network instead of
    /// repairing itself, and only an offline rescan brings it back. The
    /// default is six — the industry's settlement depth, about an hour, and
    /// deeper than any Bitcoin mainnet reorg since the March 2013 consensus
    /// bug. A hundred reproduces the old rollback journal's safety envelope
    /// exactly, at roughly sixteen hours of latency.
    #[serde(default = "Config::<A>::default_confirmation_depth")]
    pub confirmation_depth: u64,
    /// How often this chain observes a tip, and how much one pass indexes.
    ///
    /// One key, one shape, on every chain: `sdk/config` owns the document and
    /// each chain owns its section, so there is no reason left for one chain
    /// to flatten what another nests.
    #[serde(default)]
    pub cadence: Cadence,
}

impl<A: Ord> Config<A> {
    /// The three values with no sensible default; everything else takes the
    /// chain's own.
    #[must_use]
    pub fn new(network: Network, rpc: Rpc, asset: A) -> Self {
        Self {
            network,
            rpc,
            asset,
            address_type: AddressType::default(),
            limits: Limits::default(),
            confirmation_depth: Self::default_confirmation_depth(),
            cadence: Cadence::new(),
        }
    }

    /// The chain-neutral identity of the configured network, without
    /// connecting: no node is contacted and no storage is opened.
    #[must_use]
    pub fn network(&self) -> base::Network {
        self.network.canonical()
    }

    /// Rejects the value on its own: no node, no storage, no file.
    ///
    /// Everything an in-code builder can get wrong is checked here rather than
    /// inside a `Deserialize` implementation, so both ways in reach the same
    /// refusal.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] for an unusable endpoint list, a
    /// retry policy whose first wait exceeds its own cap, a zero
    /// synchronization bound, a confirmation depth outside
    /// `1..=`[`IndexerParams::DEPTH_LIMIT`], or fee ceilings this chain
    /// cannot price under.
    pub fn validate(&self) -> Result<(), Error> {
        self.rpc.validate()?;
        self.cadence.validate()?;
        let limit = IndexerParams::<(), ()>::DEPTH_LIMIT;
        if self.confirmation_depth == 0 || self.confirmation_depth > limit {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                format!("Bitcoin confirmation depth must be between 1 and {limit}"),
            ));
        }
        self.wallet().validate()?;
        Ok(())
    }

    /// Validates the value, then verifies the node's network. Nothing is
    /// opened and nothing is written.
    ///
    /// # Errors
    ///
    /// [`Self::validate`]'s refusals, and whatever `RpcClient::connect`
    /// reports when the node serves another network.
    pub async fn connect(self, secrets: &dyn json_rpc::Secrets) -> Result<ChainHandle<A>, Error> {
        self.connect_monitored(secrets, &mut Vec::new(), logging::Logger::default(), None)
            .await
    }

    async fn connect_monitored(
        self,
        secrets: &dyn json_rpc::Secrets,
        statistics: &mut Vec<json_rpc::Statistics>,
        logger: logging::Logger,
        transport: Option<Transport>,
    ) -> Result<ChainHandle<A>, Error> {
        self.validate()?;
        let transport = match transport {
            Some(transport) => transport,
            None => {
                let (transport, endpoints) = self.rpc.logged(secrets, &logger)?;
                statistics.extend(endpoints);
                transport
            }
        };
        let rpc = self.verified(transport).await?;
        Ok(ChainHandle { config: self, rpc })
    }

    /// The node identity checks [`Self::connect`] runs over `transport`: the
    /// node serves this configuration's network.
    async fn verified(&self, transport: Transport) -> Result<RpcClient<Transport>, Error> {
        RpcClient::connect(transport, &self.network).await
    }

    const fn default_confirmation_depth() -> u64 {
        IndexerParams::<(), ()>::CONFIRMED_DEPTH
    }
}

impl<A> Config<A> {
    /// The unit of every asset this chain declares. Bitcoin has one asset,
    /// and [`BTC`] states what an amount of it is; nothing here is
    /// configurable, and no wallet carries it.
    #[must_use]
    pub fn units(&self) -> Units {
        let network = self.network.canonical();
        Units::from_iter([((network, AssetKind::Native.id()), Unit::native(&BTC))])
    }

    /// The wallet configuration this deployment's factory is built from: the
    /// network, the address type, and the fee ceilings. One function, so
    /// [`Config::validate`] refuses exactly what [`ChainHandle::compose`] would
    /// build.
    fn wallet(&self) -> WalletConfig {
        WalletConfig::new(&self.network)
            .with_address_type(self.address_type)
            .with_fee_target_blocks(self.limits.fee_target_blocks)
            .with_max_fee_rate(FeeRate::from_sat_per_kvb(self.limits.max_fee_rate))
    }
}

/// The ceilings one Bitcoin deployment prices and builds transactions under.
///
/// Both are safety bounds an operator chooses beside `confirmation_depth`:
/// how far ahead the node is asked to price a confirmation, and the fee rate
/// above which this deployment refuses to build or submit anything at all.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Confirmation target of the node's fee estimate, in blocks.
    pub fee_target_blocks: u16,
    /// Largest accepted fee rate, in satoshis per kilo-virtual-byte — Core's
    /// own `maxfeerate` unit. Above [`FeeRate::CORE_LIMIT`] no node would
    /// accept the submission, so the value is refused.
    pub max_fee_rate: u64,
}

impl Default for Limits {
    /// The chain's own documented ceilings: a
    /// [`WalletConfig::FEE_TARGET_BLOCKS`]-block estimate and Core's default
    /// fee ceiling [`FeeRate::DEFAULT_MAXIMUM`].
    fn default() -> Self {
        Self {
            fee_target_blocks: WalletConfig::FEE_TARGET_BLOCKS,
            max_fee_rate: FeeRate::DEFAULT_MAXIMUM.sat_per_kvb(),
        }
    }
}

impl<A: FromStr> Config<A> {
    /// This chain's own tag text, in the application's tag type, for a section
    /// that names no asset.
    ///
    /// # Panics
    ///
    /// When `A` cannot represent `"btc"`. Such an application states `asset`
    /// explicitly.
    fn default_asset() -> A {
        A::from_str("btc")
            .unwrap_or_else(|_| panic!("the default Bitcoin asset tag must be a tag of this type"))
    }
}

/// A checked configuration and the client connected under it, to a node
/// whose network identity has been verified. It builds this chain's parts
/// once storage is open.
pub struct ChainHandle<A> {
    config: Config<A>,
    rpc: RpcClient<Transport>,
}

impl<A> ChainHandle<A> {
    /// The chain-neutral identity of the network the node serves, which is
    /// the repository [`Self::compose`] needs.
    #[must_use]
    pub fn network(&self) -> base::Network {
        self.config.network.canonical()
    }

    /// Everything the application registers for this chain: the index, already
    /// mirroring its watch list to `repository`, the wallet factories under
    /// their asset tags, and the cadence this chain polls at.
    ///
    /// Bitcoin finishes every send before its call returns, so `registrar` is
    /// unused here; it is an argument on every chain so an application passes
    /// it uniformly.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] when the index or the wallet
    /// factory refuses the configured values, and whatever the repository
    /// reports when its stored network disagrees with this one.
    pub async fn compose<R>(
        self,
        repository: R,
        _registrar: &Arc<dyn SubmissionRegistrar>,
    ) -> Result<ComposedChain<A>, Error>
    where
        R: Store,
    {
        let Self { config, rpc } = self;
        let units = config.units();
        let params = IndexerParams {
            sync: config.cadence.policy,
            confirmed_depth: config.confirmation_depth,
            ..IndexerParams::new(rpc.clone(), repository.clone())
        };
        let indexer = Arc::new(crate::indexer(params)?);
        indexer.checkpoint(&config.network.canonical()).await?;
        let wallet = config.wallet();
        // The repository's own answers, each completed by the blocks this
        // index holds above its confirmed line. Paired here because this is
        // the place that holds both: the rows are the store's, the pending
        // blocks are the index's, and the index answers no query itself.
        let reads = Arc::new(indexing::PendingJoin::new(repository, indexer.clone()));
        // The unspent rows with the pending blocks folded in, so change is
        // spendable at depth one. The store itself refuses them while the
        // address's own history is still being read.
        let factory = WalletFactory::new(wallet, &rpc, reads.clone())?;
        Ok(ComposedChain {
            index: indexer,
            history: reads.clone(),
            balance: reads,
            factories: vec![(config.asset, Box::new(factory) as Box<_>)],
            units,
            poll: config.cadence.poll,
        })
    }
}

/// [`ChainHandle::compose`] with this chain's type erased, so one vector holds every
/// configured chain and [`sdk_config::Composition`] folds them without naming any
/// of them. An application writes none of this.
impl<A: Send + 'static, R: Store> Connection<A, R> for ChainHandle<A> {
    fn compose<'a>(
        self: Box<Self>,
        repository: R,
        registrar: &'a Arc<dyn SubmissionRegistrar>,
    ) -> base::BoxFuture<'a, Result<ComposedChain<A>, sdk_config::Error>> {
        Box::pin(async move {
            (*self)
                .compose(repository, registrar)
                .await
                .map_err(sdk_config::Error::refused)
        })
    }
}

/// The registration hook: a document's `"bitcoin"` section, parsed by this
/// crate and handed back as something `sdk/config` can hold beside the other
/// chains without naming any of them.
///
/// An application writes none of this, and an embedder who builds a [`Config`]
/// in code never reaches it: the same three steps run either way, and only one
/// of them involves a file.
impl<A, R> sdk_config::Chain<A, R> for Config<A>
where
    A: Clone + Ord + Send + Sync + 'static,
    R: Store,
{
    fn network(&self) -> base::Network {
        Config::network(self)
    }

    fn accept(&self) -> Result<Option<String>, sdk_config::Error> {
        self.validate().map_err(sdk_config::Error::refused)?;
        Ok(Some(format!(
            "bitcoin/{}: committing {} blocks below the node tip; a reorg deeper \
             than that quarantines this network and needs an offline rescan",
            self.network().name,
            self.confirmation_depth,
        )))
    }

    fn verify<'a>(
        &'a self,
        transport: &'a Transport,
    ) -> base::BoxFuture<'a, Result<(), sdk_config::Error>> {
        Box::pin(async move {
            self.verified(transport.clone())
                .await
                .map(drop)
                .map_err(sdk_config::Error::refused)
        })
    }

    fn connect<'a>(
        &'a self,
        secrets: &'a dyn json_rpc::Secrets,
        statistics: &'a mut Vec<json_rpc::Statistics>,
        logger: logging::Logger,
        transport: Option<Transport>,
    ) -> base::BoxFuture<'a, Result<Box<dyn Connection<A, R>>, sdk_config::Error>> {
        Box::pin(async move {
            let node = self
                .clone()
                .connect_monitored(secrets, statistics, logger, transport)
                .await
                .map_err(sdk_config::Error::refused)?;
            Ok(Box::new(node) as Box<dyn Connection<A, R>>)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use futures_executor::block_on;
    use serde_json::json;

    use super::*;
    use crate::native::rpc::script::{Reply, Script, Scripted};

    fn parse(value: serde_json::Value) -> Result<Config<String>, Error> {
        serde_json::from_value(value)
            .map_err(|error| Error::new(ErrorKind::InvalidConfiguration, error.to_string()))
    }

    /// Each node states its own credentials and bounds; the indexing policy
    /// is a sibling key and never reaches the transport.
    #[test]
    fn bitcoin_rpc_configuration_is_independent_of_indexing_policy() {
        let mut config = parse(json!({
            "network": "regtest",
            "rpc": {
                "endpoints": [
                    {
                        "url": "http://127.0.0.1:18443",
                        "headers": {"Authorization": "Basic dGVzdA=="},
                        "bounds": {"timeout_ms": 4000}
                    },
                    {
                        "url": "http://127.0.0.1:18444",
                        "bounds": {"max_response_bytes": 8192}
                    }
                ]
            },
            "confirmation_depth": 3, "cadence": {"batch_size": 7}
        }))
        .unwrap();
        config.validate().unwrap();
        assert_eq!(config.asset, "btc");
        assert_eq!(config.rpc.endpoint_count(), 2);
        let secrets = BTreeMap::new();
        config.rpc.transport(&secrets).expect("both nodes resolve");
        assert_eq!(config.rpc.endpoints[0].bounds.timeout_ms, 4_000);
        assert_eq!(config.rpc.endpoints[1].bounds.max_response_bytes, 8_192);
        assert_eq!(config.network().name, "regtest");
        assert_eq!(config.confirmation_depth, 3);
        assert_eq!(config.cadence.policy.batch_size, 7);
        config.confirmation_depth = 9;
        config.cadence.policy.batch_size = 19;
        config
            .rpc
            .transport(&secrets)
            .expect("indexing policy does not reach the transport");
        let cadence = config.cadence;
        config.rpc.endpoints.clear();
        assert!(config.rpc.transport(&secrets).is_err());
        assert_eq!(config.cadence, cadence);
    }

    /// What an amount of Bitcoin's one asset is, declared by the chain and
    /// nowhere else.
    #[test]
    fn units_declare_the_one_asset_this_chain_has() {
        let config = parse(json!({
            "network": "regtest",
            "rpc": {"endpoints": ["http://127.0.0.1:18443"]}
        }))
        .expect("the smallest section");

        let units = config.units();

        let btc = units
            .of(&config.network(), &AssetKind::Native.id())
            .expect("BTC");
        assert_eq!((btc.decimals, btc.ticker.as_deref()), (8, Some("BTC")));
        assert_eq!(btc.name.as_deref(), Some("Bitcoin"));
    }

    /// The point of the types being the API: a value assembled in Rust and the
    /// same value read from a document are the same value, defaults included.
    #[test]
    fn a_value_built_in_code_carries_the_same_defaults_as_a_parsed_one() {
        let url = "http://127.0.0.1:18443";
        let built = Config::new(Network::Regtest, Rpc::endpoint(url), "btc".to_owned());
        let read = parse(json!({"network": "regtest", "rpc": {"endpoints": [url]}}))
            .expect("the smallest section");
        assert_eq!(built.asset, read.asset);
        assert_eq!(built.network, read.network);
        assert_eq!(built.confirmation_depth, read.confirmation_depth);
        assert_eq!(built.address_type, read.address_type);
        assert_eq!(built.limits, read.limits);
        assert_eq!(built.cadence, read.cadence);
        built.validate().expect("the defaults are usable");
    }

    /// The address type and the fee ceiling are deployment choices, so they
    /// are reachable from the only surface a deployment has. Before this key
    /// existed, `WalletConfig::with_address_type` and `with_max_fee_rate` were
    /// unreachable from `Config`, and the ceiling `WalletConfig::validate`
    /// checks as a safety bound was one no operator could set.
    #[test]
    fn the_address_type_and_the_fee_ceilings_reach_the_wallet_configuration() {
        let section = |value: serde_json::Value| {
            let mut section = json!({
                "network": "regtest",
                "rpc": { "endpoints": ["http://127.0.0.1:18443"] }
            });
            for (key, value) in value.as_object().expect("an object") {
                section[key] = value.clone();
            }
            parse(section)
        };
        let default = section(json!({})).expect("the smallest section");
        assert_eq!(default.address_type, AddressType::SegwitV0);
        assert_eq!(default.limits.fee_target_blocks, 6);
        assert_eq!(
            default.limits.max_fee_rate,
            FeeRate::DEFAULT_MAXIMUM.sat_per_kvb()
        );
        assert_eq!(default.wallet(), WalletConfig::new(&Network::Regtest));

        let chosen = section(json!({
            "address_type": "taproot",
            "limits": {"fee_target_blocks": 2, "max_fee_rate": 500_000}
        }))
        .expect("both deployment choices");
        chosen.validate().expect("a usable pair of ceilings");
        assert_eq!(
            chosen.wallet(),
            WalletConfig::new(&Network::Regtest)
                .with_address_type(AddressType::Taproot)
                .with_fee_target_blocks(2)
                .with_max_fee_rate(FeeRate::from_sat_per_kvb(500_000))
        );

        // Both are validated on the value alone, with no node and no storage.
        for limits in [
            json!({"fee_target_blocks": 0, "max_fee_rate": 500_000}),
            json!({"fee_target_blocks": 6, "max_fee_rate": 0}),
            json!({"fee_target_blocks": 6, "max_fee_rate": 100_000_001}),
        ] {
            assert_eq!(
                section(json!({"limits": limits}))
                    .expect("a well-formed section")
                    .validate()
                    .expect_err("an unusable ceiling")
                    .message,
                "Bitcoin wallet network, fee target, and fee ceiling must be valid"
            );
        }
        assert!(section(json!({"limits": {"fee_target_blocks": 6}})).is_err());
        assert!(section(json!({"address_type": "p2pkh"})).is_err());
    }

    /// Bitcoin is the one chain that keeps a confirmation depth, because it is
    /// the one chain whose node cannot state a finality line. The key is
    /// optional, defaults to six, and is bounded on both sides.
    #[test]
    fn the_confirmation_depth_defaults_to_six_and_is_bounded() {
        let section = |depth: Option<u64>| {
            let mut value = json!({
                "network": "regtest",
                "rpc": { "endpoints": ["http://127.0.0.1:18443"] }
            });
            if let Some(depth) = depth {
                value["confirmation_depth"] = json!(depth);
            }
            parse(value).expect("a well-formed section")
        };
        let limit = IndexerParams::<(), ()>::DEPTH_LIMIT;
        assert_eq!(section(None).confirmation_depth, 6);
        section(None).validate().expect("the default is accepted");
        for depth in [1, 6, 100, limit] {
            section(Some(depth))
                .validate()
                .expect("a depth inside the accepted range");
        }
        for depth in [0, limit + 1] {
            assert_eq!(
                section(Some(depth))
                    .validate()
                    .expect_err("an unusable depth")
                    .message,
                format!("Bitcoin confirmation depth must be between 1 and {limit}")
            );
        }
    }

    /// The retired key names a rollback journal that no longer exists; a stale
    /// configuration fails loudly instead of being ignored.
    #[test]
    fn the_retired_retention_key_is_rejected() {
        let error = parse(json!({
            "network": "regtest",
            "rpc": { "endpoints": ["http://127.0.0.1:18443"] },
            "reorg_retention": 100
        }))
        .err()
        .expect("a retired key");
        assert!(
            error.message.starts_with("unknown field `reorg_retention`"),
            "{error}"
        );
    }

    #[test]
    fn the_asset_tag_is_configurable() {
        let config = parse(json!({
            "network": "regtest", "asset": "btc-regtest",
            "rpc": { "endpoints": ["http://127.0.0.1:18443"] },
            "confirmation_depth": 1
        }))
        .expect("an explicit tag");
        assert_eq!(config.asset, "btc-regtest");
    }

    /// [`sdk_config::Chain::verify`] over a scripted node: the identity check
    /// `connect` runs, and the one a replacement node list must pass.
    fn verify(config: &Config<String>, script: &Script) -> Result<(), sdk_config::Error> {
        let transport: Transport = Arc::new(script.clone());
        let chain: &dyn sdk_config::Chain<String, indexing::memory::Repository> = config;
        block_on(chain.verify(&transport))
    }

    fn regtest() -> Config<String> {
        let rpc = Rpc::endpoint("http://127.0.0.1:18443");
        Config::new(Network::Regtest, rpc, "btc".to_owned())
    }

    fn node_info() -> Reply {
        Reply::ok("getnetworkinfo", json!({"version": 310_000})).expecting(json!([]))
    }

    fn chain_info(chain: &str) -> Reply {
        let info = json!({
            "chain": chain, "pruned": false, "initialblockdownload": false,
            "blocks": 10, "headers": 10, "bestblockhash": "02".repeat(32),
        });
        Reply::ok("getblockchaininfo", info).expecting(json!([]))
    }

    #[test]
    fn verify_accepts_a_node_serving_the_configured_network() {
        let script = Script::node([
            node_info(),
            chain_info("regtest"),
            Reply::ok(
                "getindexinfo",
                json!({"txindex": {"synced": true, "best_block_height": 10}}),
            )
            .expecting(json!(["txindex"])),
        ]);

        verify(&regtest(), &script).expect("a regtest node serves a regtest section");

        script.exhausted();
    }

    /// A mainnet node behind a regtest section is refused on its chain name,
    /// before the transaction index is consulted.
    #[test]
    fn verify_refuses_a_node_serving_another_network() {
        let script = Script::node([node_info(), chain_info("main")]);

        let error = verify(&regtest(), &script).expect_err("another network");

        assert_eq!(error.kind, sdk_config::ErrorKind::Section);
        assert_eq!(
            error.message,
            "Bitcoin RPC network does not match configuration"
        );
        script.exhausted();
    }
}
