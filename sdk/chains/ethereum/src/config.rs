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

use std::{collections::BTreeSet, str::FromStr, sync::Arc};

use base::{Networked as _, Unit, Units};
use indexing::Checkpoint as _;
use sdk_config::{ComposedChain, Connection};
use serde::{Deserialize, de};
use wallets::SubmissionRegistrar;

/// What a store must answer for **this** chain to compose over it.
///
/// [`indexing::Repository`] is the minimum any index needs — registration and
/// committing — and the rest is what this chain's own consumers read: the
/// balance fold, and the repository's own history query
/// ([`indexing::ConfirmedHistory`]) that this chain joins to its overlay so
/// `apps/server` can project a page and its sender can prove a transaction absent.
/// A trait bound, never a backend: this file names no store implementation. No
/// [`indexing::OutputStore`]: an account chain produces no UTXO records, and
/// asking for one would make a store carry a table it can never fill.
pub trait Store:
    indexing::Repository
    + indexing::BalanceStore
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
/// `sdk/config` names no chain type, so this file adds Ethereum's own keys
/// around the shell and implements [`sdk_config::Chain`] for the result.
use sdk_config::{Cadence, Rpc, Transport};

use crate::{
    IndexerParams, Network, RpcClient, WalletConfig, WalletFactory,
    native::{Address, AssetKind, ETH, Error, ErrorKind, transaction::FeeLimits},
};

/// Everything one Ethereum network needs. Fields are public; [`Config::new`]
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
    /// The stable application name of the network, such as `mainnet`.
    pub network: String,
    /// The EIP-155 chain ID the node is checked against.
    pub chain_id: u64,
    /// The node, or the ordered failover list, and how much a call repeats.
    pub rpc: Rpc,
    /// The asset tag of this network's ETH wallets.
    #[serde(default = "Config::<A>::default_asset")]
    pub asset: A,
    /// The ceilings this deployment builds transactions under.
    #[serde(default)]
    pub limits: Limits,
    /// Allowlisted ERC-20 tokens, each under its own asset tag. Every token
    /// factory shares ETH's send pipeline, so one address never reuses a nonce
    /// across its assets.
    #[serde(default)]
    pub tokens: Vec<Token<A>>,
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
            network: network.slug,
            chain_id: network.chain_id,
            rpc,
            asset,
            limits: Limits::default(),
            tokens: Vec::new(),
            cadence: Cadence::new(),
        }
    }

    /// The chain-neutral identity of the configured network, without
    /// connecting: no node is contacted and no storage is opened.
    #[must_use]
    pub fn network(&self) -> base::Network {
        Network::new(self.chain_id, self.network.as_str()).canonical()
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
    /// synchronization bound, a blank slug or zero chain ID, a token list that
    /// spans failover endpoints, a repeated asset tag, an unusable token, or
    /// fee ceilings that contradict each other.
    pub fn validate(&self) -> Result<(), Error> {
        self.rpc.validate()?;
        self.cadence.validate()?;
        Network::new(self.chain_id, self.network.as_str()).validate()?;
        if !self.tokens.is_empty() && self.rpc.endpoint_count() != 1 {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Ethereum token configuration currently requires exactly one RPC endpoint",
            ));
        }
        let mut tags = BTreeSet::from([&self.asset]);
        for token in &self.tokens {
            token.validate()?;
            if !tags.insert(&token.asset) {
                return Err(Error::new(
                    ErrorKind::InvalidConfiguration,
                    "every Ethereum asset tag must be unique",
                ));
            }
        }
        self.limits.build()?;
        Ok(())
    }

    /// Validates the value, then verifies the node's chain ID, that it serves
    /// block traces, and every allowlisted token contract's code and
    /// configured decimals. Nothing is opened and nothing is written.
    ///
    /// The trace probe is not optional and has no opt-out: contract-initiated
    /// ether is ordinary on Ethereum, and a node that cannot report it would
    /// index balances that are quietly wrong. A node that fails it fails this
    /// call, and an application that checks every chain's identity before it
    /// opens anything therefore starts no chain at all — the probe is a
    /// deployment refusal, not an Ethereum-only quarantine.
    ///
    /// # Errors
    ///
    /// [`Self::validate`]'s refusals, and whatever the node reports when it
    /// serves another chain ID, cannot trace a block, or disagrees with a
    /// configured token.
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
    /// node serves this chain id, traces calls, and every allowlisted token
    /// is a contract with the configured decimals.
    async fn verified(&self, transport: Transport) -> Result<RpcClient<Transport>, Error> {
        let network = Network::new(self.chain_id, self.network.as_str());
        let rpc = RpcClient::connect(transport, &network).await?;
        rpc.validate_tracing().await?;
        for token in &self.tokens {
            rpc.validate_token(&token.contract, token.decimals).await?;
        }
        Ok(rpc)
    }
}

impl<A> Config<A> {
    /// The unit of every asset this chain declares: ether, from [`ETH`], and
    /// each allowlisted token in the scale [`Config::connect`] checked its
    /// contract's own `decimals()` against.
    ///
    /// An ERC-20 is the authority on its own scale, and this is where that
    /// authority is published. No wallet carries it.
    #[must_use]
    pub fn units(&self) -> Units {
        let network = Network::new(self.chain_id, self.network.as_str()).canonical();
        let ether = (AssetKind::Native.id(), Unit::native(&ETH));
        let tokens = self.tokens.iter().map(|token| {
            (
                AssetKind::Erc20(token.contract.clone()).id(),
                Unit::token(token.decimals.into()),
            )
        });
        std::iter::once(ether)
            .chain(tokens)
            .map(|(asset, unit)| ((network.clone(), asset), unit))
            .collect()
    }
}

impl<A: FromStr> Config<A> {
    /// This chain's own tag text, in the application's tag type, for a section
    /// that names no asset.
    ///
    /// # Panics
    ///
    /// When `A` cannot represent `"eth"`. Such an application states `asset`
    /// explicitly.
    fn default_asset() -> A {
        A::from_str("eth")
            .unwrap_or_else(|_| panic!("the default Ethereum asset tag must be a tag of this type"))
    }
}

/// A checked configuration and the client connected under it, to a node
/// whose chain identity has been verified. It builds this chain's parts once
/// storage is open.
pub struct ChainHandle<A> {
    config: Config<A>,
    rpc: RpcClient<Transport>,
}

impl<A> ChainHandle<A> {
    /// The chain-neutral identity of the network the node serves, which is
    /// the repository [`Self::compose`] needs.
    #[must_use]
    pub fn network(&self) -> base::Network {
        self.rpc.network().canonical()
    }

    /// Everything the application registers for this chain: the index, already
    /// mirroring its watch list to `repository`, the wallet factories under
    /// their asset tags, and the cadence this chain polls at.
    ///
    /// Ethereum finishes every send before its call returns, so `registrar` is
    /// unused here; it is an argument on every chain so an application passes
    /// it uniformly.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] when the index or a wallet factory
    /// refuses the configured values, and whatever the repository reports when
    /// its stored network disagrees with this one.
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
        let network = rpc.network().clone();
        let params = IndexerParams {
            sync: config.cadence.policy,
            ..IndexerParams::new(rpc.clone(), repository.clone())
        };
        let indexer = Arc::new(crate::indexer(params)?);
        indexer.checkpoint(&network.canonical()).await?;
        let wallet = WalletConfig::new(&network).with_fee_limits(config.limits.build()?);
        let eth = WalletFactory::new(wallet.clone(), &rpc)?;
        let mut factories: Vec<(A, Box<dyn wallets::WalletFactory>)> = Vec::new();
        for token in config.tokens {
            let settings = wallet.clone().with_token(token.contract, token.decimals);
            factories.push((token.asset, Box::new(eth.sibling(settings)?)));
        }
        factories.insert(0, (config.asset, Box::new(eth)));
        // The repository's own confirmed pages and balance fold, each
        // completed by the blocks this index holds above its confirmed line.
        // Ethereum's own wallets read neither; `apps/server` does, and a chain
        // is the place holding both halves of one network's answer: the rows
        // are the store's, the pending blocks are the index's, and the index
        // answers no query itself.
        let reads = Arc::new(indexing::PendingJoin::new(repository, indexer.clone()));
        Ok(ComposedChain {
            index: indexer,
            history: reads.clone(),
            balance: reads,
            factories,
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

/// One allowlisted ERC-20 token: its contract, the asset tag its wallets use,
/// and the decimals the contract is checked against when this chain connects.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields, bound = "A: serde::de::DeserializeOwned")]
pub struct Token<A> {
    /// The contract address, in canonical lowercase when it is written as
    /// text.
    #[serde(deserialize_with = "Token::<A>::deserialize_contract")]
    pub contract: Address,
    /// The asset tag this token's wallets are registered under.
    pub asset: A,
    /// What the contract's own `decimals()` must report.
    pub decimals: u8,
}

impl<A> Token<A> {
    /// The widest scale an ERC-20 amount is read in. A contract's decimals are
    /// read from the chain when this chain connects, so an out-of-range value
    /// is a configuration error rather than a chain answer.
    pub const MAX_DECIMALS: u8 = 36;

    /// One token under one tag, in the scale its contract reports.
    #[must_use]
    pub const fn new(contract: Address, asset: A, decimals: u8) -> Self {
        Self {
            contract,
            asset,
            decimals,
        }
    }

    /// Rejects the value on its own, which a text encoding cannot check for an
    /// embedder that hands over an [`Address`] directly.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] for the zero address or decimals
    /// above [`Token::MAX_DECIMALS`].
    pub fn validate(&self) -> Result<(), Error> {
        if self.contract.is_zero() {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "token contract must not be the zero address",
            ));
        }
        if self.decimals > Self::MAX_DECIMALS {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "token decimals must be at most 36",
            ));
        }
        Ok(())
    }

    /// The deserializer owns the encoding, and only the encoding: a value an
    /// embedder builds in code has no text to check.
    fn deserialize_contract<'de, D>(deserializer: D) -> Result<Address, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        let contract = encoded.parse::<Address>().map_err(de::Error::custom)?;
        if contract.to_string() != encoded {
            return Err(de::Error::custom(
                "token contract must use canonical lowercase encoding",
            ));
        }
        Ok(contract)
    }
}

/// The transaction-construction ceilings of one deployment, as a document
/// states them: [`FeeLimits`] before its six numbers are checked against each
/// other, which [`Limits::build`] does.
pub type Limits = FeeLimits<u128>;

/// The registration hook: a document's `"ethereum"` section, parsed by this
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
        Ok(None)
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

    use std::time::Duration;

    use futures_executor::block_on;
    use serde_json::{Value, json};

    use super::*;
    use crate::native::{
        Wei,
        rpc::script::{Reply, Script, Scripted},
    };

    const CONTRACT: &str = "0x1111111111111111111111111111111111111111";
    const BLOCK_HASH: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER_CONTRACT: &str = "0x2222222222222222222222222222222222222222";

    fn section(token: Option<&str>) -> Value {
        let mut config = json!({
            "network": "mainnet",
            "chain_id": 1,
            "rpc": {
                "endpoints": ["http://127.0.0.1:8545"]
            }
        });
        if let Some(contract) = token {
            config["tokens"] = json!([{"contract": contract, "asset": "usdc", "decimals": 6}]);
        }
        config
    }

    fn parse(value: Value) -> Result<Config<String>, Error> {
        let config: Config<String> = serde_json::from_value(value)
            .map_err(|error| Error::new(ErrorKind::InvalidConfiguration, error.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// The RPC section and the flattened sync section are independent, and a
    /// node's own settings travel with that node.
    #[test]
    fn ethereum_rpc_and_cadence_fields_reach_separate_runtime_settings() {
        let secrets = BTreeMap::new();
        let config = parse(section(None)).expect("defaulted bounds");
        assert_eq!(config.rpc.endpoint_count(), 1);
        config
            .rpc
            .transport(&secrets)
            .expect("the default composes");
        assert_eq!(config.cadence.policy.batch_size, 100);
        assert_eq!(config.cadence.poll, Duration::from_secs(1));

        let mut value = section(None);
        value["rpc"] = json!({
            "endpoints": [
                {
                    "url": "http://127.0.0.1:9545",
                    "headers": {"x-fixture": "first"},
                    "bounds": {"timeout_ms": 3000}
                },
                {
                    "url": "http://127.0.0.1:8545",
                    "headers": {"x-fixture": "second"},
                    "bounds": {"max_response_bytes": 8192}
                }
            ],
            "repeat": {"attempts": 2, "backoff_ms": 10, "max_backoff_ms": 20}
        });
        value["cadence"] = json!({"poll_millis": 17, "batch_size": 7});
        let config = parse(value).expect("explicit bounds");
        assert_eq!(config.rpc.endpoint_count(), 2);
        config
            .rpc
            .transport(&secrets)
            .expect("two endpoints with their own settings compose");
        assert_eq!(config.cadence.policy.batch_size, 7);
        assert_eq!(config.cadence.poll, Duration::from_millis(17));
    }

    /// The point of the types being the API: a value assembled in Rust and the
    /// same value read from a document are the same value, defaults included.
    /// What an amount of each configured asset is, declared by the chain and
    /// nowhere else: ether's own 18 digits, and every token's in the scale
    /// `connect` checks its contract's `decimals()` against.
    #[test]
    fn units_declare_ether_and_every_allowlisted_token_scale() {
        let config = parse(section(Some(CONTRACT))).expect("one token");

        let units = config.units();

        let ether = units
            .of(&config.network(), &AssetKind::Native.id())
            .expect("ether");
        assert_eq!((ether.decimals, ether.ticker.as_deref()), (18, Some("ETH")));
        let contract: Address = CONTRACT.parse().expect("a contract address");
        let token = units
            .of(&config.network(), &AssetKind::Erc20(contract).id())
            .expect("the allowlisted token");
        assert_eq!(token.decimals, u32::from(config.tokens[0].decimals));
        assert_eq!((token.name.as_ref(), token.ticker.as_ref()), (None, None));
        let other: Address = OTHER_CONTRACT.parse().expect("a contract address");
        assert_eq!(
            units.of(&config.network(), &AssetKind::Erc20(other).id()),
            None
        );
    }

    #[test]
    fn composing_networks_preserves_each_contracts_verified_scale() {
        let main = parse(section(Some(CONTRACT))).expect("mainnet config");
        let mut other = parse(section(Some(CONTRACT))).expect("other network config");
        other.network = "sepolia".to_owned();
        other.chain_id = 11_155_111;
        other.asset = "sepolia-eth".to_owned();
        other.tokens[0].asset = "sepolia-token".to_owned();
        other.tokens[0].decimals = 18;
        main.validate().expect("valid mainnet config");
        other.validate().expect("valid sepolia config");
        let mut units = main.units();
        units.extend(other.units());
        let asset = AssetKind::Erc20(CONTRACT.parse().unwrap()).id();
        assert_eq!(
            units.of(&main.network(), &asset).unwrap().decimals,
            u32::from(main.tokens[0].decimals)
        );
        assert_eq!(units.of(&other.network(), &asset).unwrap().decimals, 18);
    }

    #[test]
    fn a_value_built_in_code_carries_the_same_defaults_as_a_parsed_one() {
        let url = "http://127.0.0.1:8545";
        let built = Config::new(
            Network::new(1, "mainnet"),
            Rpc::endpoint(url),
            "eth".to_owned(),
        );
        let read = parse(section(None)).expect("the smallest section");
        assert_eq!(built.asset, read.asset);
        assert_eq!(built.network(), read.network());
        assert_eq!(built.cadence, read.cadence);
        assert!(built.tokens.is_empty() && read.tokens.is_empty());
        assert_eq!(
            built.limits.build().expect("the documented ceilings"),
            read.limits.build().expect("the same ceilings")
        );
        built.validate().expect("the defaults are usable");
    }

    /// The nested `cadence` key is a closed schema of its own, so a repeated
    /// bound is a parse failure rather than a silent last-wins — which is
    /// what `#[serde(flatten)]` cost this section before every chain agreed
    /// on one shape.
    #[test]
    fn a_nested_cadence_rejects_raw_duplicate_keys_and_keeps_rpc_error_precedence() {
        for (field, first, second) in [("poll_millis", 1, 2), ("batch_size", 3, 4)] {
            let mut value = section(None);
            value["cadence"] = json!({});
            let raw = serde_json::to_string(&value).unwrap().replace(
                r#""cadence":{}"#,
                &format!(r#""cadence":{{"{field}":{first},"{field}":{second}}}"#),
            );
            serde_json::from_str::<Value>(&raw).expect("a Value alone would collapse duplicates");
            let error = serde_json::from_str::<Config<String>>(&raw)
                .err()
                .expect("duplicate cadence field");
            assert!(
                error
                    .to_string()
                    .starts_with(&format!("duplicate field `{field}`")),
                "{error}"
            );
        }

        let mut config: Config<String> = serde_json::from_value(section(None)).unwrap();
        config.rpc.endpoints.clear();
        config.cadence.policy = indexing::SyncPolicy::new(0);
        assert_eq!(
            config.validate().unwrap_err().message,
            "invalid database or RPC configuration"
        );
        config
            .rpc
            .endpoints
            .push(json_rpc::Endpoint::new("http://127.0.0.1:8545"));
        assert_eq!(
            config.validate().unwrap_err().message,
            "index polling and batch values must be positive"
        );
    }

    /// The `limits` section is six ceilings under six keys, and the keys are
    /// the document's contract: `gas_margin_basis_points` is the one a
    /// deployment's file states. The six are checked against each other when
    /// the value is validated, not when it is parsed.
    #[test]
    fn limits_section_states_six_ceilings_and_contradictory_ones_are_refused() {
        let stated = json!({
            "max_input_bytes": 1024,
            "gas_margin_basis_points": 1500,
            "max_gas_limit": 1_000_000,
            "max_fee_per_gas": 200,
            "max_priority_fee_per_gas": 100,
            "max_total_fee": 300
        });
        let mut value = section(None);
        value["limits"] = stated.clone();
        let config = parse(value).expect("consistent ceilings");
        assert_eq!(
            config.limits.build().expect("the stated ceilings"),
            FeeLimits::new(
                1024,
                1500,
                1_000_000,
                Wei::from_u128(200),
                Wei::from_u128(100),
                Wei::from_u128(300),
            )
            .expect("the same ceilings")
        );

        let mut inverted = section(None);
        inverted["limits"] = stated.clone();
        inverted["limits"]["max_priority_fee_per_gas"] = json!(201);
        let error = parse(inverted)
            .err()
            .expect("a tip ceiling above the fee ceiling");
        assert_eq!(error.kind, ErrorKind::InvalidConfiguration);
        assert_eq!(
            error.message,
            "Ethereum priority-fee ceiling must not exceed the max-fee ceiling"
        );

        for (absent, present, expected) in [
            (
                "gas_margin_basis_points",
                "gas_limit_margin_basis_points",
                "unknown field `gas_limit_margin_basis_points`",
            ),
            (
                "max_total_fee",
                "max_input_bytes",
                "missing field `max_total_fee`",
            ),
        ] {
            let mut keys = stated.clone();
            let moved = keys.as_object_mut().and_then(|keys| keys.remove(absent));
            keys[present] = moved.expect("a stated ceiling");
            let mut value = section(None);
            value["limits"] = keys;
            let error = serde_json::from_value::<Config<String>>(value)
                .err()
                .expect("a section with other keys");
            assert!(error.to_string().starts_with(expected), "{error}");
        }
    }

    #[test]
    fn accepts_every_canonical_nonzero_token_under_its_own_tag() {
        let config = parse(section(Some(CONTRACT))).expect("valid token configuration");
        let [token] = &config.tokens[..] else {
            panic!("one configured token");
        };
        assert_eq!(token.contract.to_string(), CONTRACT);
        assert_eq!(token.asset, "usdc");
        assert_eq!(token.decimals, 6);
        assert_eq!(config.asset, "eth");

        let mut several = section(Some(CONTRACT));
        several["tokens"] = json!([
            {"contract": CONTRACT, "asset": "usdc", "decimals": 6},
            {"contract": OTHER_CONTRACT, "asset": "dai", "decimals": 18}
        ]);
        let config = parse(several).expect("several tokens share one Ethereum network");
        assert_eq!(config.tokens.len(), 2);
        assert_eq!(config.tokens[1].decimals, 18);
    }

    #[test]
    fn rejects_duplicate_native_and_token_asset_tags() {
        let mut native = section(Some(CONTRACT));
        native["tokens"][0]["asset"] = json!("eth");
        assert_eq!(
            parse(native).err().expect("one tag per factory").message,
            "every Ethereum asset tag must be unique"
        );

        let mut repeated = section(Some(CONTRACT));
        repeated["tokens"] = json!([
            {"contract": CONTRACT, "asset": "usdc", "decimals": 6},
            {"contract": OTHER_CONTRACT, "asset": "usdc", "decimals": 18}
        ]);
        assert_eq!(
            parse(repeated).err().expect("one tag per factory").message,
            "every Ethereum asset tag must be unique"
        );
    }

    #[test]
    fn requires_an_explicit_tag_and_in_range_decimals_for_every_token() {
        let mut untagged = section(Some(CONTRACT));
        untagged["tokens"] = json!([{"contract": CONTRACT, "decimals": 6}]);
        assert!(parse(untagged).is_err(), "a token names its own asset tag");

        let mut undecided = section(Some(CONTRACT));
        undecided["tokens"] = json!([{"contract": CONTRACT, "asset": "usdc"}]);
        assert!(
            parse(undecided).is_err(),
            "a token states the decimals its contract is checked against"
        );

        let mut absurd = section(Some(CONTRACT));
        absurd["tokens"][0]["decimals"] = json!(37);
        assert_eq!(
            parse(absurd).err().expect("bounded decimals").message,
            "token decimals must be at most 36"
        );
    }

    /// The encoding is the deserializer's; the value is `validate`'s. A zero
    /// contract an embedder assembles in Rust never passes through text, so
    /// the check that catches it must be on the value.
    #[test]
    fn rejects_malformed_noncanonical_and_zero_token_contracts() {
        for (contract, expected) in [
            ("0x1234", "Ethereum address must contain exactly 20 bytes"),
            (
                "0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                "token contract must use canonical lowercase encoding",
            ),
            (
                "0x0000000000000000000000000000000000000000",
                "token contract must not be the zero address",
            ),
        ] {
            let error = parse(section(Some(contract)))
                .err()
                .expect("invalid contract must be rejected");
            assert_eq!(error.message, expected);
        }
        let zero = Token::new(Address::from_bytes([0; 20]), "usdc".to_owned(), 6);
        assert_eq!(
            zero.validate()
                .expect_err("built in code, never parsed")
                .message,
            "token contract must not be the zero address"
        );
    }

    #[test]
    fn requires_one_endpoint_for_endpoint_affine_token_validation() {
        let mut value = section(Some(CONTRACT));
        value["rpc"]["endpoints"] = json!(["http://127.0.0.1:8545", "http://127.0.0.1:9545"]);
        assert_eq!(
            parse(value)
                .err()
                .expect("token validation must not span failover endpoints")
                .message,
            "Ethereum token configuration currently requires exactly one RPC endpoint"
        );

        let mut native_only = section(None);
        native_only["rpc"]["endpoints"] = json!(["http://127.0.0.1:8545", "http://127.0.0.1:9545"]);
        parse(native_only).expect("native Ethereum keeps ordered RPC failover");
    }

    /// [`sdk_config::Chain::verify`] over a scripted node: the identity check
    /// `connect` runs, and the one a replacement node list must pass.
    fn verify(config: &Config<String>, script: &Script) -> Result<(), sdk_config::Error> {
        let transport: Transport = Arc::new(script.clone());
        let chain: &dyn sdk_config::Chain<String, indexing::memory::Repository> = config;
        block_on(chain.verify(&transport))
    }

    /// The node's latest block, empty: the trace probe still traces it.
    fn latest() -> Value {
        json!({
            "hash": BLOCK_HASH,
            "parentHash": "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "sha3Uncles": "0xdddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            "miner": "0x0000000000000000000000000000000000000000",
            "stateRoot": "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            "transactionsRoot":
                "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "receiptsRoot": "0xabababababababababababababababababababababababababababababababab",
            "logsBloom": format!("0x{}", "00".repeat(256)),
            "difficulty": "0x0",
            "number": "0xa",
            "gasLimit": "0x1c9c380",
            "gasUsed": "0x0",
            "timestamp": "0x64",
            "extraData": "0x",
            "mixHash": "0xcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
            "nonce": "0x0000000000000000",
            "uncles": [],
            "transactions": []
        })
    }

    #[test]
    fn verify_accepts_a_node_serving_the_configured_network() {
        let script = Script::node([
            Reply::ok("eth_chainId", json!("0x1")).expecting(json!([])),
            Reply::ok("eth_getBlockByNumber", latest()).expecting(json!(["latest", true])),
            Reply::ok("debug_traceBlockByHash", json!([])).expecting(json!([
                BLOCK_HASH,
                {"tracer": "prestateTracer", "tracerConfig": {"diffMode": true}}
            ])),
        ]);
        let config = parse(section(None)).expect("a chain-1 section");

        verify(&config, &script).expect("a chain-1 node serves a chain-1 section");

        script.exhausted();
    }

    /// An Anvil node behind a mainnet section is refused on its chain ID,
    /// before the trace probe.
    #[test]
    fn verify_refuses_a_node_serving_another_network() {
        let script = Script::node([Reply::ok("eth_chainId", json!("0x7a69"))]);
        let config = parse(section(None)).expect("a chain-1 section");

        let error = verify(&config, &script).expect_err("another chain");

        assert_eq!(error.kind, sdk_config::ErrorKind::Section);
        assert_eq!(
            error.message,
            "Ethereum RPC chain ID 31337 does not match configured chain ID 1"
        );
        script.exhausted();
    }
}
