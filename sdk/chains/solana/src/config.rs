//! What this chain needs to run, and how it assembles itself from it.
//!
//! The three steps are the same on every chain: a [`Config`] value — built in
//! code or deserialized from any `serde` format — [`Config::connect`]s to a
//! node and verifies its identity, and the resulting [`ChainHandle`] then
//! [`ChainHandle::compose`]s over an opened repository into a
//! [`wallets::ComposedChain`]. No storage is opened on the first step, which is
//! what lets an application check every chain's identity before it opens
//! anything. [`wallets::Composition`] is what
//! holds this chain beside the others.

use std::{collections::BTreeSet, str::FromStr, sync::Arc};

use base::{Networked as _, Unit, Units};
use indexing::Checkpoint as _;
use serde::{Deserialize, de};
use wallets::{ComposedChain, Connection, SubmissionRegistrar};

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
/// `sdk/config` names no chain type, so this file adds Solana's own keys
/// around the shell and implements [`sdk_config::Chain`] for the result.
use sdk_config::{Cadence, Rpc, Transport};

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
#[derive(Deserialize)]
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
    /// The node, and how much a call repeats. Solana reads carry context
    /// floors and a submission must not be repeated against a second node, so
    /// the list holds exactly one.
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
    /// endpoint list, anything but exactly one endpoint, a retry policy whose
    /// first wait exceeds its own cap, a zero synchronization bound, a
    /// repeated asset tag, or an unusable token.
    pub fn validate(&self) -> Result<(), Error> {
        self.cluster().validate()?;
        self.rpc.validate()?;
        if self.rpc.endpoint_count() != 1 {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Solana requires exactly one RPC endpoint",
            ));
        }
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
    pub async fn connect(self, secrets: &dyn json_rpc::Secrets) -> Result<ChainHandle<A>, Error> {
        self.connect_monitored(secrets, &mut Vec::new()).await
    }

    async fn connect_monitored(
        self,
        secrets: &dyn json_rpc::Secrets,
        statistics: &mut Vec<json_rpc::Statistics>,
    ) -> Result<ChainHandle<A>, Error> {
        self.validate()?;
        let network = self.cluster();
        let (transport, endpoints) = self.rpc.monitored(secrets)?;
        statistics.extend(endpoints);
        let rpc = RpcClient::connect(transport, &network).await?;
        for token in &self.tokens {
            rpc.validate_token(&token.mint, token.decimals).await?;
        }
        Ok(ChainHandle { config: self, rpc })
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
            ..IndexerParams::new(rpc.clone(), repository.clone())
        };
        let indexer = Arc::new(crate::indexer(params)?);
        indexer.checkpoint(&network.canonical()).await?;
        // The repository's own confirmed pages and balance fold, each
        // completed by the blocks this index holds above its confirmed line.
        // Paired here because this is the place that holds both halves — the
        // rows are the store's, the pending blocks are the index's, and the
        // index answers no query itself — and handed to whoever reads a page:
        // the sender's absence proof, and `apps/server` through `ComposedChain`.
        let reads = Arc::new(wallets::PendingJoin::new(repository, indexer.clone()));
        let history: Arc<dyn wallets::History> = reads.clone();
        let wallet = WalletConfig::new(&network);
        let sol = WalletFactory::new(wallet.clone(), &rpc, &indexer, history.clone())?
            .with_registrar(registrar.clone());
        let mut factories: Vec<(A, Box<dyn wallets::WalletFactory>)> = Vec::new();
        for token in config.tokens {
            let settings = wallet.clone().with_token(token.mint, token.decimals);
            factories.push((token.asset, Box::new(sol.sibling(settings)?)));
        }
        factories.insert(0, (config.asset, Box::new(sol)));
        Ok(ComposedChain {
            index: indexer,
            history,
            balance: reads,
            factories,
            units,
            poll: config.cadence.poll,
        })
    }
}

/// [`ChainHandle::compose`] with this chain's type erased, so one vector holds
/// every configured chain and [`wallets::Composition`] folds them without
/// naming any of them. An application writes none of this.
impl<A: Send + 'static, R: Store> Connection<A, R> for ChainHandle<A> {
    fn compose<'a>(
        self: Box<Self>,
        repository: R,
        registrar: &'a Arc<dyn SubmissionRegistrar>,
    ) -> base::BoxFuture<'a, Result<ComposedChain<A>, wallets::WalletError>> {
        Box::pin(async move { Ok((*self).compose(repository, registrar).await?) })
    }
}

/// One allowlisted SPL token: its mint, the asset tag its wallets use, and the
/// decimals the mint account is checked against when this chain connects.
#[derive(Deserialize)]
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
        Ok(None)
    }

    fn connect<'a>(
        self: Box<Self>,
        secrets: &'a dyn json_rpc::Secrets,
        statistics: &'a mut Vec<json_rpc::Statistics>,
    ) -> base::BoxFuture<'a, Result<Box<dyn Connection<A, R>>, sdk_config::Error>> {
        Box::pin(async move {
            let node = (*self)
                .connect_monitored(secrets, statistics)
                .await
                .map_err(sdk_config::Error::refused)?;
            Ok(Box::new(node) as Box<dyn Connection<A, R>>)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, time::Duration};

    use serde_json::{Value, json};

    use super::*;

    const GENESIS: &str = "11111111111111111111111111111111";
    const MINT: &str = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU";
    const OTHER_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    fn parse(value: Value) -> Result<Config<String>, Error> {
        let config: Config<String> = serde_json::from_value(value)
            .map_err(|error| Error::new(ErrorKind::InvalidConfiguration, error.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// Solana shares the one RPC shape and adds exactly one rule of its own:
    /// one node. The retired singular `endpoint` key is not aliased.
    #[test]
    fn solana_uses_the_one_rpc_shape_and_exactly_one_endpoint() {
        let section = |rpc: Value| {
            json!({
                "network": "localnet",
                "genesis_hash": GENESIS,
                "rpc": rpc,
                "cadence": {"poll_millis": 17, "batch_size": 7}
            })
        };
        parse(section(json!({"endpoints": [{
            "url": "http://127.0.0.1:8899",
            "bounds": {"max_request_bytes": 1048576}
        }]})))
        .expect("one endpoint with its own bounds");
        assert!(
            parse(section(json!({"endpoint": "http://127.0.0.1:8899"}))).is_err(),
            "the singular key is replaced, not aliased"
        );
        for endpoints in [json!([]), json!(["http://a", "http://b"])] {
            let error = parse(section(json!({"endpoints": endpoints})))
                .err()
                .expect("Solana reads carry context floors");
            assert!(
                error.message == "Solana requires exactly one RPC endpoint"
                    || error.message == "invalid database or RPC configuration",
                "{error}"
            );
        }
        parse(section(
            json!({"endpoints": ["http://127.0.0.1:8899"], "repeat": "off"}),
        ))
        .expect("`off` states in configuration what `Request::once` enforces per call");
    }

    fn token_section(tokens: Value) -> Value {
        json!({
            "network": "localnet",
            "genesis_hash": GENESIS,
            "rpc": {"endpoints": ["http://127.0.0.1:8899"]},
            "tokens": tokens,
            "cadence": {"poll_millis": 17, "batch_size": 7}
        })
    }

    /// What an amount of each configured asset is, declared by the chain and
    /// nowhere else: SOL's own 9 digits, and every mint's in the scale
    /// `connect` checks the mint account against.
    #[test]
    fn units_declare_sol_and_every_allowlisted_mint_scale() {
        let config = parse(token_section(
            json!([{"mint": MINT, "asset": "usdc", "decimals": 6}]),
        ))
        .expect("one allowlisted SPL token");

        let units = config.units();

        let sol = units
            .of(&config.network(), &AssetKind::Native.id())
            .expect("SOL");
        assert_eq!((sol.decimals, sol.ticker.as_deref()), (9, Some("SOL")));
        let mint: Address = MINT.parse().expect("a canonical mint");
        let token = units
            .of(&config.network(), &AssetKind::SplToken(mint).id())
            .expect("the allowlisted mint");
        assert_eq!(token.decimals, u32::from(config.tokens[0].decimals));
        assert_eq!((token.name.as_ref(), token.ticker.as_ref()), (None, None));
        let other: Address = OTHER_MINT.parse().expect("a canonical mint");
        assert_eq!(
            units.of(&config.network(), &AssetKind::SplToken(other).id()),
            None
        );
    }

    /// The point of the types being the API: a value assembled in Rust and the
    /// same value read from a document are the same value, defaults included.
    #[test]
    fn a_value_built_in_code_carries_the_same_defaults_as_a_parsed_one() {
        let url = "http://127.0.0.1:8899";
        let network = Network::new("localnet", GENESIS.parse().expect("canonical Base58"));
        let built = Config::new(network, Rpc::endpoint(url), "sol".to_owned());
        let read = parse(json!({
            "network": "localnet", "genesis_hash": GENESIS, "rpc": {"endpoints": [url]}
        }))
        .expect("the smallest section");
        assert_eq!(built.asset, read.asset);
        assert_eq!(built.network(), read.network());
        assert_eq!(built.genesis_hash, read.genesis_hash);
        assert_eq!(built.cadence, read.cadence);
        built.validate().expect("the defaults are usable");
    }

    #[test]
    fn every_canonical_mint_registers_under_its_own_asset_tag() {
        let config = parse(token_section(
            json!([{"mint": MINT, "asset": "usdc", "decimals": 6}]),
        ))
        .expect("one allowlisted SPL token");
        let [token] = &config.tokens[..] else {
            panic!("one configured token");
        };
        assert_eq!(token.mint.to_string(), MINT);
        assert_eq!(token.asset, "usdc");
        assert_eq!(token.decimals, 6);
        assert_eq!(config.asset, "sol");

        let several = parse(token_section(json!([
            {"mint": MINT, "asset": "usdc", "decimals": 6},
            {"mint": OTHER_MINT, "asset": "usdt", "decimals": 9}
        ])))
        .expect("several mints share one cluster");
        assert_eq!(several.tokens.len(), 2);
        assert_eq!(several.tokens[1].decimals, 9);

        assert!(
            parse(token_section(json!([])))
                .expect("no token at all")
                .tokens
                .is_empty(),
            "the section stays native-only by default"
        );
    }

    /// The encoding is the deserializer's; the value is `validate`'s. A System
    /// program mint an embedder assembles in Rust never passes through text,
    /// so the check that catches it must be on the value.
    #[test]
    fn a_misconfigured_token_is_refused_before_any_chain_call() {
        for (tokens, expected) in [
            (
                json!([{"mint": MINT, "asset": "sol", "decimals": 6}]),
                "every Solana asset tag must be unique",
            ),
            (
                json!([
                    {"mint": MINT, "asset": "usdc", "decimals": 6},
                    {"mint": OTHER_MINT, "asset": "usdc", "decimals": 9}
                ]),
                "every Solana asset tag must be unique",
            ),
            (
                json!([{"mint": MINT, "asset": "usdc", "decimals": 20}]),
                "Solana token decimals must be at most 19",
            ),
            (
                json!([{"mint": GENESIS, "asset": "u", "decimals": 6}]),
                "token mint must not be the System program address",
            ),
            (
                json!([{"mint": "0xabc", "asset": "usdc", "decimals": 6}]),
                "Solana address contains invalid Base58",
            ),
            (
                json!([{"mint": "11111111111111111111111111111", "asset": "u", "decimals": 6}]),
                "Solana address must contain exactly 32 bytes",
            ),
        ] {
            let error = parse(token_section(tokens))
                .err()
                .expect("refused configuration");
            assert!(error.message.starts_with(expected), "{error}");
        }

        for tokens in [
            json!([{"mint": MINT, "decimals": 6}]),
            json!([{"mint": MINT, "asset": "usdc"}]),
            json!([{"asset": "usdc", "decimals": 6}]),
            json!([{"mint": MINT, "asset": "usdc", "decimals": 6, "program": "token-2022"}]),
        ] {
            assert!(
                parse(token_section(tokens)).is_err(),
                "a token names exactly its mint, tag, and decimals"
            );
        }
    }

    /// Every chain writes this section the same way now: one nested `cadence`
    /// key with a closed schema and the shared shell's defaults, so an omitted
    /// bound is the constructor's own rather than a parse failure.
    #[test]
    fn cadence_defaults_its_bounds_and_rejects_retired_keys() {
        for retired in [
            json!({"confirmation_depth": 1, "poll_millis": 17, "batch_size": 7}),
            json!({"reorg_retention": 10, "poll_millis": 17, "batch_size": 7}),
            json!({"poll_millis": 17, "batch_size": 7, "repeat": false}),
            json!({"poll_millis": null, "batch_size": 7}),
            json!({"poll_millis": 17, "batch_size": "7"}),
        ] {
            let mut section = token_section(json!([]));
            section["cadence"] = retired;
            assert!(parse(section).is_err(), "a closed synchronization schema");
        }
        let mut defaulted = token_section(json!([]));
        defaulted
            .as_object_mut()
            .expect("a section object")
            .remove("cadence");
        let config = parse(defaulted).expect("both bounds default");
        assert_eq!(config.cadence, Cadence::new());
    }

    #[test]
    fn configured_values_reach_runtime_and_rpc_errors_still_precede_cadence_errors() {
        let mut config = parse(json!({
            "network": "localnet",
            "genesis_hash": GENESIS,
            "rpc": {
                "endpoints": [{
                    "url": "http://127.0.0.1:8899",
                    "bounds": {"max_request_bytes": 1048576, "max_response_bytes": 8192}
                }],
                "repeat": "off"
            },
            "cadence": {"poll_millis": 17, "batch_size": 7}
        }))
        .expect("explicit bounds");
        config
            .rpc
            .transport(&BTreeMap::new())
            .expect("one endpoint, no decorator");
        assert_eq!(config.network().name, "localnet");
        assert_eq!(config.cadence.policy.batch_size, 7);
        assert_eq!(config.cadence.poll, Duration::from_millis(17));

        config.rpc.endpoints.clear();
        config.cadence.policy = indexing::SyncPolicy::new(0);
        assert_eq!(
            config.validate().unwrap_err().message,
            "invalid database or RPC configuration"
        );
        config
            .rpc
            .endpoints
            .push(json_rpc::Endpoint::new("http://127.0.0.1:8899"));
        assert_eq!(
            config.validate().unwrap_err().message,
            "index polling and batch values must be positive"
        );
    }
}
