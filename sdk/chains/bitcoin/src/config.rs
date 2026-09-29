//! What this chain needs to run, and how it assembles itself from it.
//!
//! The three steps are the same on every chain: a [`Config`] value — built in
//! code or deserialized from any `serde` format — [`Config::connect`]s to a
//! node and verifies its identity, and the verified [`Node`] then
//! [`Node::compose`]s over an opened repository into a
//! [`wallets::ComposedChain`]. No storage is opened on the first step, which is
//! what lets an application check every chain's identity before it opens
//! anything (`docs/design/030 §8 (ex-028)` section 10.1). [`wallets::Composition`] is what
//! holds this chain beside the others.

use std::{str::FromStr, sync::Arc};

use base::{Networked as _, Unit, Units};
use indexing::Checkpoint as _;
use serde::Deserialize;
use wallets::{ComposedChain, Connection, SubmissionRegistrar};

/// What a store must answer for **this** chain to compose over it.
///
/// [`indexing::Repository`] is the minimum any index needs — registration and
/// committing — and the rest is what Bitcoin's own consumers read: the balance
/// fold, the repository's own history query ([`indexing::ConfirmedHistory`])
/// that this chain joins to its overlay for the page `apps/api` projects, and
/// the live outputs coin selection spends. Trait bounds, never a backend: this
/// file names no store implementation. Named here rather than in `wallets`,
/// because it is this
/// chain's requirement and not a rule about stores: an account chain needs no
/// [`indexing::OutputStore`] at all, and a third party's sink that answers none
/// of these still indexes (`docs/design/030 §8 (ex-032)`, gap 1).
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
#[derive(Deserialize)]
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
    /// to flatten what another nests (`docs/CHAIN_LAYOUT.md` section 6).
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
    /// refusal (`docs/design/030 §8 (ex-028)` section 5.4).
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
    pub async fn connect(self, secrets: &dyn json_rpc::Secrets) -> Result<Node<A>, Error> {
        self.validate()?;
        let transport = self.rpc.transport(secrets)?;
        let rpc = RpcClient::connect(transport, &self.network).await?;
        Ok(Node { config: self, rpc })
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
        Units::from_iter([(AssetKind::Native.id(), Unit::native(&BTC))])
    }

    /// The wallet configuration this deployment's factory is built from: the
    /// network, the address type, and the fee ceilings. One function, so
    /// [`Config::validate`] refuses exactly what [`Node::compose`] would
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

/// A Bitcoin node whose network identity has been verified. It holds the
/// checked configuration and the connected client, and builds this chain's
/// parts once storage is open.
pub struct Node<A> {
    config: Config<A>,
    rpc: RpcClient<Transport>,
}

impl<A> Node<A> {
    /// The chain-neutral identity of the network this node serves, which is
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
            ..IndexerParams::new(rpc.clone(), repository)
        };
        let indexer = Arc::new(crate::indexer(params)?);
        indexer.checkpoint(&config.network.canonical()).await?;
        let wallet = config.wallet();
        // The repository's own unspent rows, with this index's pending blocks
        // folded in and its two gates taken, so change is spendable at depth one
        // and nothing is offered while the address's own history is still being
        // read. A reader of its own, not a capability of the index: the rows are
        // the store's, and what this adds is the half the store cannot know.
        let outputs = Arc::new(indexer.outputs());
        let factory = WalletFactory::new(wallet, &rpc, outputs)?;
        // The repository's own confirmed pages joined to this index's overlay:
        // the only value that holds both halves is the one just built.
        let history = Arc::new(indexer.history());
        // A reader of its own for the same reason: the fold is the store's own
        // total, completed by this index's overlay, and a consumer asks the
        // fold rather than the indexer that happens to hold both halves.
        let balance = Arc::new(indexer.balances());
        Ok(ComposedChain {
            index: indexer,
            history,
            balance,
            factories: vec![(config.asset, Box::new(factory) as Box<_>)],
            units,
            poll: config.cadence.poll,
        })
    }
}

/// [`Node::compose`] with this chain's type erased, so one vector holds every
/// configured chain and [`wallets::Composition`] folds them without naming any
/// of them. An application writes none of this.
impl<A: Send + 'static, R: Store> Connection<A, R> for Node<A> {
    fn compose<'a>(
        self: Box<Self>,
        repository: R,
        registrar: &'a Arc<dyn SubmissionRegistrar>,
    ) -> base::BoxFuture<'a, Result<ComposedChain<A>, wallets::WalletError>> {
        Box::pin(async move { Ok((*self).compose(repository, registrar).await?) })
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
    A: Ord + Send + Sync + 'static,
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

    fn connect<'a>(
        self: Box<Self>,
        secrets: &'a dyn json_rpc::Secrets,
    ) -> base::BoxFuture<'a, Result<Box<dyn Connection<A, R>>, sdk_config::Error>> {
        Box::pin(async move {
            let node = Config::connect(*self, secrets)
                .await
                .map_err(sdk_config::Error::refused)?;
            Ok(Box::new(node) as Box<dyn Connection<A, R>>)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;

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

        let btc = units.of(&AssetKind::Native.id()).expect("BTC");
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
}
