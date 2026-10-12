//! What this chain needs to run, and how it assembles itself from it.
//!
//! The three steps are the same on every chain: a [`Config`] value — built in
//! code or deserialized from any `serde` format — [`Config::connect`]s to a
//! node and verifies its identity, and the resulting [`ChainHandle`] then
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
/// [`indexing::Repository`], the minimum any index needs. A trait bound,
/// never a backend.
pub trait Store: indexing::Repository + Clone + Send + Sync + 'static {}

impl<R> Store for R where R: indexing::Repository + Clone + Send + Sync + 'static {}

/// The shell every chain's configuration shares, from `payments::config`: the nodes
/// and their repetition, the cadence this chain indexes at, and the
/// registration hook that lets a document hand this file its own section.
/// `payments::config` names no chain type, so this file adds Solana's own keys
/// around the shell and implements [`payments::config::Chain`] for the result.
use payments::config::{Cadence, Rpc, Transport};

use crate::{
    IndexerParams, Network, RpcClient, WalletConfig, WalletFactory,
    native::{Address, AssetKind, Error, ErrorKind, GenesisHash, SOL},
};

/// Everything one Solana cluster needs. Fields are public; [`Config::new`]
/// fills the chain defaults, and `serde` fills an absent key by calling the
/// same functions, so a value built in code and a value read from a document
/// cannot drift.
///
/// `A` is the asset tag the application keys its wallet factories by; the
/// chain never interprets it.
///
/// The cluster's genesis hash is configured and verified, never discovered:
/// [`Config::connect`] asks the node for its own and refuses a mismatch.
#[derive(Clone, Deserialize)]
#[serde(
    deny_unknown_fields,
    bound = "A: serde::de::DeserializeOwned + FromStr + Ord"
)]
pub struct Config<A> {
    /// The stable application name of the cluster, such as `localnet`.
    pub network: String,
    /// The genesis hash the node is checked against.
    #[serde(deserialize_with = "Config::<A>::deserialize_genesis_hash")]
    pub genesis_hash: GenesisHash,
    /// The nodes, one or more. Whatever `repeat` configures, no Solana call
    /// is repeated: a read reaches one node, because it carries a context
    /// floor another node may not have reached, and a submission is broadcast
    /// to every node the transport holds, each exactly once.
    pub rpc: Rpc,
    /// The asset tag of this cluster's SOL wallets.
    #[serde(default = "Config::<A>::default_asset")]
    pub asset: A,
    /// Allowlisted SPL tokens, each under its own asset tag. Every token
    /// factory shares SOL's send pipeline, so one address is never reserved
    /// twice across its assets.
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
            genesis_hash: network.genesis_hash().clone(),
            network: network.slug().to_owned(),
            rpc,
            asset,
            tokens: Vec::new(),
            cadence: Cadence::new(),
        }
    }

    /// The chain-neutral identity of the configured cluster, without
    /// connecting: no node is contacted and no storage is opened.
    #[must_use]
    pub fn network(&self) -> base::Network {
        self.cluster().canonical()
    }

    /// Rejects the value on its own: no node, no storage, no file.
    ///
    /// Everything an in-code builder can get wrong is checked here rather than
    /// inside a `Deserialize` implementation, so both ways in reach the same
    /// refusal.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] for a blank slug, an unusable
    /// endpoint list, a retry policy whose first wait exceeds its own cap, a
    /// zero synchronization bound, a repeated asset tag, or an unusable token.
    pub fn validate(&self) -> Result<(), Error> {
        self.cluster().validate()?;
        self.rpc.validate()?;
        self.cadence.validate()?;
        let mut tags = BTreeSet::from([&self.asset]);
        for token in &self.tokens {
            token.validate()?;
            if !tags.insert(&token.asset) {
                return Err(Error::new(
                    ErrorKind::InvalidConfiguration,
                    "every Solana asset tag must be unique",
                ));
            }
        }
        Ok(())
    }

    /// Validates the value, then verifies the cluster's genesis hash and every
    /// allowlisted mint's program and decimals. Nothing is opened and nothing
    /// is written.
    ///
    /// A mint this build cannot index — a Token-2022 one, whose confidential
    /// transfers need not report an amount — refuses the deployment here
    /// rather than indexing as zero.
    ///
    /// # Errors
    ///
    /// [`Self::validate`]'s refusals, and whatever the node reports when it
    /// serves another cluster or disagrees with a configured mint.
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
    /// node serves this cluster's genesis hash with its Memo program, and
    /// every allowlisted mint has the configured decimals.
    async fn verified(&self, transport: Transport) -> Result<RpcClient<Transport>, Error> {
        let rpc = RpcClient::connect(transport, &self.cluster()).await?;
        for token in &self.tokens {
            rpc.validate_token(&token.mint, token.decimals).await?;
        }
        Ok(rpc)
    }

    /// The cluster these two keys name.
    fn cluster(&self) -> Network {
        Network::new(self.network.as_str(), self.genesis_hash.clone())
    }
}

impl<A> Config<A> {
    /// The unit of every asset this chain declares: SOL, from [`SOL`], and
    /// each allowlisted mint in the scale [`Config::connect`] checked the mint
    /// account against.
    ///
    /// A mint is the authority on its own scale, and this is where that
    /// authority is published. No wallet carries it.
    #[must_use]
    pub fn units(&self) -> Units {
        let network = Network::new(self.network.as_str(), self.genesis_hash.clone()).canonical();
        let sol = (AssetKind::Native.id(), Unit::native(&SOL));
        let tokens = self.tokens.iter().map(|token| {
            (
                AssetKind::SplToken(token.mint.clone()).id(),
                Unit::token(token.decimals.into()),
            )
        });
        std::iter::once(sol)
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
    /// When `A` cannot represent `"sol"`. Such an application states `asset`
    /// explicitly.
    fn default_asset() -> A {
        A::from_str("sol")
            .unwrap_or_else(|_| panic!("the default Solana asset tag must be a tag of this type"))
    }

    /// The deserializer owns the encoding, and only the encoding: a value an
    /// embedder builds in code hands over a [`GenesisHash`] that is already
    /// canonical.
    fn deserialize_genesis_hash<'de, D>(deserializer: D) -> Result<GenesisHash, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse::<GenesisHash>()
            .map_err(de::Error::custom)
    }
}

/// A checked configuration and the client connected under it, to a node
/// whose cluster identity has been verified. It builds this chain's parts
/// once storage is open.
pub struct ChainHandle<A> {
    config: Config<A>,
    rpc: RpcClient<Transport>,
}

impl<A> ChainHandle<A> {
    /// The chain-neutral identity of the cluster its node serves, which is
    /// the repository [`Self::compose`] needs.
    #[must_use]
    pub fn network(&self) -> base::Network {
        self.rpc.network().canonical()
    }

    /// Everything the application registers for this chain: the index, already
    /// mirroring its watch list to `repository`, the wallet factories under
    /// their asset tags, and the cadence this chain polls at.
    ///
    /// Solana is the one chain that finishes send work after its call returns,
    /// so its factories hand that work to `registrar` instead of spawning it:
    /// the application decides how long it may run and drains it at shutdown.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] when the index or a wallet factory
    /// refuses the configured values, and whatever the repository reports when
    /// its stored network disagrees with this one.
    pub async fn compose<R>(
        self,
        repository: R,
        registrar: &Arc<dyn SubmissionRegistrar>,
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
        let wallet = WalletConfig::new(&network);
        let sol = WalletFactory::new(wallet.clone(), &rpc)?.with_registrar(registrar.clone());
        let mut factories: Vec<(A, Box<dyn wallets::WalletFactory>)> = Vec::new();
        for token in config.tokens {
            let settings = wallet.clone().with_token(token.mint, token.decimals);
            factories.push((token.asset, Box::new(sol.sibling(settings)?)));
        }
        factories.insert(0, (config.asset, Box::new(sol)));
        Ok(ComposedChain {
            pending: indexer.clone(),
            index: indexer,
            factories,
            units,
        })
    }
}

/// [`ChainHandle::compose`] with this chain's type erased, so one vector holds
/// every configured chain and [`payments::config::Composition`] folds them without
/// naming any of them. An application writes none of this.
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

/// One allowlisted SPL token: its mint, the asset tag its wallets use, and the
/// decimals the mint account is checked against when this chain connects.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields, bound = "A: serde::de::DeserializeOwned")]
pub struct Token<A> {
    /// The mint address.
    #[serde(deserialize_with = "Token::<A>::deserialize_mint")]
    pub mint: Address,
    /// The asset tag this token's wallets are registered under.
    pub asset: A,
    /// What the mint account must report.
    pub decimals: u8,
}

impl<A> Token<A> {
    /// The widest scale an SPL amount is read in: the amount is 64 bits, so a
    /// mint of more decimals could not name one whole token. The chain remains
    /// the authority — connecting reads the mint account and refuses decimals
    /// that disagree with it.
    pub const MAX_DECIMALS: u8 = 19;

    /// One token under one tag, in the scale its mint reports.
    #[must_use]
    pub const fn new(mint: Address, asset: A, decimals: u8) -> Self {
        Self {
            mint,
            asset,
            decimals,
        }
    }

    /// Rejects the value on its own, which a text encoding cannot check for an
    /// embedder that hands over an [`Address`] directly.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] for the System program address or
    /// decimals above [`Token::MAX_DECIMALS`].
    pub fn validate(&self) -> Result<(), Error> {
        if self.mint == Address::from_bytes([0; 32]) {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "token mint must not be the System program address",
            ));
        }
        if self.decimals > Self::MAX_DECIMALS {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Solana token decimals must be at most 19",
            ));
        }
        Ok(())
    }

    /// The deserializer owns the encoding, and only the encoding: a value an
    /// embedder builds in code has no text to check.
    fn deserialize_mint<'de, D>(deserializer: D) -> Result<Address, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse::<Address>()
            .map_err(de::Error::custom)
    }
}

/// The registration hook: a document's `"solana"` section, parsed by this
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
