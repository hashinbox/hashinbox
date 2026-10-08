//! What this chain needs to run, and how it assembles itself from it.
//!
//! The three steps are the same on every chain: a [`Config`] value — built in
//! code or deserialized from any `serde` format — [`Config::connect`]s to a
//! node and verifies its identity, and the verified [`ChainHandle`] then
//! [`ChainHandle::compose`]s over an opened repository into a
//! [`payments::config::ComposedChain`]. No storage is opened on the first step, which is
//! what lets an application check every chain's identity before it opens
//! anything. [`payments::config::Composition`] is what
//! holds this chain beside the others.

use std::{collections::BTreeSet, str::FromStr, sync::Arc};

use base::{Networked as _, Unit, Units};
use indexing::Checkpoint as _;
use payments::config::{ComposedChain, Connection};
use serde::{Deserialize, de};
use wallets::SubmissionRegistrar;

/// What a store must answer for **this** chain to compose over it:
/// [`indexing::Repository`], the minimum any index needs — registration and
/// committing. A trait bound, never a backend.
pub trait Store: indexing::Repository + Clone + Send + Sync + 'static {}

impl<R> Store for R where R: indexing::Repository + Clone + Send + Sync + 'static {}

/// The shell every chain's configuration shares, from `payments::config`: the nodes
/// and their repetition, the cadence this chain indexes at, and the
/// registration hook that lets a document hand this file its own section.
/// `payments::config` names no chain type, so this file adds Ethereum's own keys
/// around the shell and implements [`payments::config::Chain`] for the result.
use payments::config::{Cadence, Rpc, Transport};

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
    /// One key, one shape, on every chain: `payments::config` owns the document and
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
    pub async fn connect(self) -> Result<ChainHandle<A>, Error> {
        self.connect_monitored(&mut Vec::new(), logging::Logger::default(), None)
            .await
    }

    async fn connect_monitored(
        self,
        statistics: &mut Vec<json_rpc::Statistics>,
        logger: logging::Logger,
        transport: Option<Transport>,
    ) -> Result<ChainHandle<A>, Error> {
        self.validate()?;
        let transport = match transport {
            Some(transport) => transport,
            None => {
                let (transport, endpoints) = self.rpc.logged(&logger)?;
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
            schedule: config
                .cadence
                .poll
                .map_or(IndexerParams::<(), ()>::POLL, indexing::Schedule::every),
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
        Ok(ComposedChain {
            pending: indexer.clone(),
            index: indexer,
            factories,
            units,
        })
    }
}

/// [`ChainHandle::compose`] with this chain's type erased, so one vector holds every
/// configured chain and [`payments::config::Composition`] folds them without naming any
/// of them. An application writes none of this.
impl<A: Send + 'static, R: Store> Connection<A, R> for ChainHandle<A> {
    fn compose<'a>(
        self: Box<Self>,
        repository: R,
        registrar: &'a Arc<dyn SubmissionRegistrar>,
    ) -> base::BoxFuture<'a, Result<ComposedChain<A>, payments::config::Error>> {
        Box::pin(async move {
            (*self)
                .compose(repository, registrar)
                .await
                .map_err(payments::config::Error::refused)
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
/// crate and handed back as something `payments::config` can hold beside the other
/// chains without naming any of them.
///
/// An application writes none of this, and an embedder who builds a [`Config`]
/// in code never reaches it: the same three steps run either way, and only one
/// of them involves a file.
impl<A, R> payments::config::Chain<A, R> for Config<A>
where
    A: Clone + Ord + Send + Sync + 'static,
    R: Store,
{
    fn network(&self) -> base::Network {
        Config::network(self)
    }

    fn accept(&self) -> Result<Option<String>, payments::config::Error> {
        self.validate().map_err(payments::config::Error::refused)?;
        Ok(None)
    }

    fn verify<'a>(
        &'a self,
        transport: &'a Transport,
    ) -> base::BoxFuture<'a, Result<(), payments::config::Error>> {
        Box::pin(async move {
            self.verified(transport.clone())
                .await
                .map(drop)
                .map_err(payments::config::Error::refused)
        })
    }

    fn connect<'a>(
        &'a self,
        statistics: &'a mut Vec<json_rpc::Statistics>,
        logger: logging::Logger,
        transport: Option<Transport>,
    ) -> base::BoxFuture<'a, Result<Box<dyn Connection<A, R>>, payments::config::Error>> {
        Box::pin(async move {
            let node = self
                .clone()
                .connect_monitored(statistics, logger, transport)
                .await
                .map_err(payments::config::Error::refused)?;
            Ok(Box::new(node) as Box<dyn Connection<A, R>>)
        })
    }
}
