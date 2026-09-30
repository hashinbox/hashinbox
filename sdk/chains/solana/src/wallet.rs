//! Solana wallets: [`WalletConfig`], the [`WalletFactory`] that makes them
//! (`wallets::WalletFactory`), and the wallet itself.

use std::sync::Arc;

use base::{
    Broadcaster, Decimal, Networked, SecretBytes, SignFuture, SignRequest, Signer, Submission,
    TransactionError, TransactionFuture, TransactionSnapshot,
};
use indexing::{Holder, Indexer};
use wallets::{
    AddressParser, Authorship, Dependency, FactoryProfile, FutureResult, Generation, Origin,
    SingleSender, TransactionFactory, Wallet as WalletContract, WalletError, WalletErrorKind,
};

use crate::native::{
    Address, AssetKind, CHAIN_NAME, Error, ErrorKind, KeyPair, Network, SOL,
    rpc::{Accounts, Client},
};
use crate::sender::native_transfer;
use crate::sender::{
    CommitSignal, FactorySender, Outbox, Recovery, Reservations, SendRequest, Spawner,
};
use crate::{BlockInterpreter, Source, TransferBuilder};

/// Which asset one factory's wallets hold on which cluster.
///
/// [`WalletConfig::new`] selects native SOL; [`WalletConfig::with_token`]
/// selects one SPL mint instead, whose decimals the node checks against the
/// mint account before a deployment opens. Fees come from the node
/// (`getFeeForMessage`) per transfer and are always paid in SOL, so there is
/// no fee policy to configure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalletConfig {
    pub(crate) network: base::Network,
    pub(crate) asset: AssetKind,
    pub(crate) decimals: u32,
}

impl WalletConfig {
    /// Native SOL on `network`, 9 decimals.
    #[must_use]
    pub fn new(network: &Network) -> Self {
        Self {
            network: network.canonical(),
            asset: AssetKind::Native,
            decimals: SOL.decimals,
        }
    }

    /// Holds the SPL token of `mint`, which has `decimals` decimals.
    #[must_use]
    pub fn with_token(mut self, mint: Address, decimals: u8) -> Self {
        self.asset = AssetKind::SplToken(mint);
        self.decimals = decimals.into();
        self
    }

    /// The index network of every wallet with this configuration.
    #[must_use]
    pub const fn network(&self) -> &base::Network {
        &self.network
    }

    /// The asset wallets of this configuration hold.
    #[must_use]
    pub const fn asset(&self) -> &AssetKind {
        &self.asset
    }

    pub(crate) fn validate(&self) -> Result<(), Error> {
        if self.network.chain != CHAIN_NAME || self.network.name.trim().is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Solana wallet configuration must name a Solana network",
            ));
        }
        if matches!(self.asset, AssetKind::Native) && self.decimals != SOL.decimals
            || self.decimals > u8::MAX.into()
        {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Solana wallet asset and decimals must agree",
            ));
        }
        Ok(())
    }
}

/// The lenient parse of user-supplied address text, which needs a network and
/// nothing else: no store, no node, no index, and no wallet. A caller holding
/// a [`Network`] builds the configuration in plain code and parses through it,
/// and [`WalletFactory`]'s own impl delegates here, so there is one parse on
/// this chain and no second one to disagree with it.
impl AddressParser for WalletConfig {
    /// Canonical Base58 of 32 bytes only; Solana has no other textual form.
    ///
    /// What accepting `text` proves: it is canonical Base58 — re-rendering the
    /// bytes returns the same string, so leading-zero and alphabet variants are
    /// refused — of exactly thirty-two bytes. It does **not** prove the bytes
    /// are an Ed25519 point: an off-curve (program-derived) address is a valid
    /// account identity and parses here. Whether native SOL may be sent to one
    /// is a transfer rule, checked on the send path, not an address rule. The
    /// cluster is not in the text at all, so it comes from this configuration.
    fn parse_address(&self, text: &str) -> Result<base::Address, WalletError> {
        let address = text.parse::<Address>().map_err(|_| invalid_address())?;
        Ok(address.canonical(&self.network))
    }
}

/// Makes Solana wallets of one [`WalletConfig`] over one RPC client and the
/// cluster's indexer: `wallets::WalletFactory`, registered with
/// `wallets::Wallets::register`.
///
/// The indexer is read only to settle an unknown submission outcome: its
/// checkpoint and its commit announcements. The sender's native history is the
/// third thing it needs, and it is not the indexer's to give: a page is the
/// repository's own query joined to that index's pending blocks, so this
/// factory is handed the one narrow capability it reads — `wallets::History`,
/// "page a holding's transactions from a position" — and names no store at all.
pub struct WalletFactory {
    config: WalletConfig,
    accounts: Arc<dyn Accounts>,
    outbox: Arc<Outbox>,
}

impl WalletFactory {
    /// Builds balance reads and the send pipeline over `rpc`, and the
    /// recovery of unknown outcomes over `indexer` and `history`. Submissions
    /// run on the current Tokio runtime; supply a supervising registrar with
    /// [`Self::with_registrar`]. No I/O.
    ///
    /// `history` is the sender's absence proof and nothing else: the recovery
    /// pages this cluster's indexed SOL history at one stable checkpoint,
    /// because the sender pays every fee and is therefore on its own
    /// transaction whatever it moved. It is passed in rather than read off
    /// `indexer` because confirmed-plus-pending is neither the index's alone nor
    /// the repository's — `wallets::PendingJoin` is what joins them, and the
    /// composition that owns both builds it.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] when `config`, `rpc`, and
    /// `indexer` name different networks.
    pub fn new<C, R>(
        config: WalletConfig,
        rpc: &Client<C>,
        indexer: &Arc<Indexer<Source<C>, BlockInterpreter, R>>,
        history: Arc<dyn wallets::History>,
    ) -> Result<Self, Error>
    where
        C: json_rpc::Client + 'static,
        Indexer<Source<C>, BlockInterpreter, R>: indexing::Checkpoint + 'static,
    {
        if config.network != rpc.network().canonical() || indexer.network() != &config.network {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Solana wallet configuration, RPC client, and indexer must share one network",
            ));
        }
        config.validate()?;
        let rpc = Arc::new(rpc.clone());
        let reservations = Reservations::default();
        // The commit announcements recovery waits on are allocated here, by
        // their only consumer, and registered as one of the indexer's
        // listeners: an indexing-only caller never pays for this channel, and
        // a caller's own listener is announced beside it.
        let (announce, progress) = CommitSignal::channel();
        indexer.listen(announce);
        let recovery = Recovery::new(
            rpc.clone(),
            indexer.clone(),
            history,
            config.network.clone(),
            progress,
            reservations.clone(),
        );
        let outbox = Outbox::new(
            rpc.clone(),
            rpc.clone(),
            rpc.clone(),
            Arc::new(Spawner),
            Arc::new(recovery),
            reservations,
        );
        Ok(Self {
            config,
            accounts: rpc,
            outbox: Arc::new(outbox),
        })
    }

    /// A factory for another asset of the same cluster that shares this one's
    /// RPC and send pipeline, so a wallet's SOL and its SPL tokens are one
    /// reserved sender and never submit twice at once.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] for another cluster or an
    /// inconsistent `config`.
    pub fn sibling(&self, config: WalletConfig) -> Result<Self, Error> {
        if config.network != self.config.network {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "a Solana sibling factory must use the same cluster",
            ));
        }
        config.validate()?;
        Ok(Self::assemble(
            config,
            self.accounts.clone(),
            self.outbox.clone(),
        ))
    }

    /// A factory over these exact handles.
    pub(crate) fn assemble(
        config: WalletConfig,
        accounts: Arc<dyn Accounts>,
        outbox: Arc<Outbox>,
    ) -> Self {
        Self {
            config,
            accounts,
            outbox,
        }
    }

    /// Runs submissions under `registrar` instead of spawning them, so the
    /// application can refuse new ones and drain running ones on shutdown.
    /// Call it before creating wallets: it replaces the send pipeline.
    #[must_use]
    pub fn with_registrar(mut self, registrar: Arc<dyn wallets::SubmissionRegistrar>) -> Self {
        self.outbox = Arc::new(self.outbox.with_registrar(registrar));
        self
    }

    /// Turns a local key into a wallet of this factory's asset.
    ///
    /// # Errors
    ///
    /// `Unsupported` when the configuration is invalid.
    pub fn attach(&self, key: KeyPair) -> Result<Arc<dyn WalletContract>, WalletError> {
        self.config.validate()?;
        Ok(Arc::new(self.attached(Arc::new(key))))
    }

    /// The concrete wallet [`Self::attach`] hands out behind the contract.
    pub(crate) fn attached(&self, key: Arc<KeyPair>) -> Wallet {
        Wallet {
            canonical: key.address().canonical(&self.config.network),
            asset: self.config.asset.id(),
            config: self.config.clone(),
            key,
            accounts: self.accounts.clone(),
            outbox: self.outbox.clone(),
        }
    }

    /// A wallet of `secret` with its authoring capability, exactly as
    /// `wallets::Wallets::author` hands one out. The send and builder paths
    /// are reachable from an author and from nothing else, so their tests
    /// take one.
    #[cfg(test)]
    pub(crate) async fn authored(&self, secret: SecretBytes) -> Arc<dyn wallets::Author> {
        let wallet = wallets::WalletFactory::create(self, secret)
            .await
            .expect("a wallet of this factory");
        self.authorship()
            .expect("an asset this factory spends")
            .author(&wallet)
            .expect("its own wallet")
    }

    fn origin(&self) -> Origin<Fingerprint> {
        Origin::new(Fingerprint {
            config: self.config.clone(),
            accounts: Dependency::of(&self.accounts),
            outbox: Dependency::of(&self.outbox),
        })
    }
}

/// What makes two wallets interchangeable: configuration and shared handles.
#[derive(PartialEq, Eq)]
struct Fingerprint {
    config: WalletConfig,
    accounts: Dependency,
    outbox: Dependency,
}

impl wallets::WalletFactory for WalletFactory {
    fn create<'a>(&'a self, secret: SecretBytes) -> FutureResult<'a, Arc<dyn WalletContract>> {
        Box::pin(async move {
            self.config.validate()?;
            self.attach(KeyPair::import(secret)?)
        })
    }

    fn generate(&self) -> FutureResult<'_, Generation> {
        Generation::of(self, KeyPair::generate_secret().map_err(WalletError::from))
    }

    fn validate(&self, wallet: &dyn WalletContract) -> Result<(), WalletError> {
        self.origin().validate(wallet, Wallet::fingerprint)
    }
}

impl AddressParser for WalletFactory {
    /// The configuration's own parse, and nothing else; see [`WalletConfig`].
    fn parse_address(&self, text: &str) -> Result<base::Address, WalletError> {
        self.config.parse_address(text)
    }
}

impl FactoryProfile for WalletFactory {
    fn network(&self) -> &base::Network {
        &self.config.network
    }

    /// SOL, or the configured SPL token's mint.
    fn asset(&self) -> indexing::AssetId {
        self.config.asset.id()
    }

    /// Native SOL only. An SPL mint is indexed and its balance reported, and
    /// this integration builds no SPL transfer, so a token factory declares
    /// no authorship at all: its wallets are plain `wallets::Wallet`s with
    /// neither a send nor a builder on them, and `Wallets::{author,
    /// send_all}` refuse its tag at lookup. That one absence replaces every
    /// gate this file used to carry.
    fn authorship(&self) -> Option<Arc<dyn Authorship>> {
        matches!(self.config.asset, AssetKind::Native).then(|| {
            let sender = FactorySender::new(self.config.clone(), self.outbox.clone());
            Arc::new(sender) as Arc<dyn Authorship>
        })
    }
}

/// One Solana address with its factory's capabilities attached.
#[derive(Clone)]
pub(crate) struct Wallet {
    /// `key.address()` in the configured network, computed once.
    canonical: base::Address,
    /// The configured asset's identity, computed once.
    asset: indexing::AssetId,
    pub(crate) config: WalletConfig,
    pub(crate) key: Arc<KeyPair>,
    accounts: Arc<dyn Accounts>,
    pub(crate) outbox: Arc<Outbox>,
}

impl Wallet {
    fn fingerprint(&self) -> Fingerprint {
        Fingerprint {
            config: self.config.clone(),
            accounts: Dependency::of(&self.accounts),
            outbox: Dependency::of(&self.outbox),
        }
    }

    /// The send request that sends `amount` SOL from this wallet to `to`.
    ///
    /// Native SOL is the only asset this integration authors, and it is also
    /// the only one whose factory declares a `wallets::Authorship`, so
    /// nothing can reach this from a token wallet.
    pub(crate) fn request(
        &self,
        to: Address,
        amount: &Decimal,
    ) -> Result<SendRequest, WalletError> {
        let from = self.key.address().clone();
        let transfer = native_transfer(&from, to, amount, self.config.decimals)?;
        Ok(SendRequest::new(self.key.clone(), from, transfer))
    }
}

impl Holder for Wallet {
    fn address(&self) -> &base::Address {
        &self.canonical
    }

    fn asset(&self) -> &indexing::AssetId {
        &self.asset
    }
}

impl Signer for Wallet {
    fn sign<'a>(&'a self, request: SignRequest) -> SignFuture<'a> {
        Signer::sign(self.key.as_ref(), request)
    }
}

/// The one rejection of `parse_address`, whatever was wrong with the text.
fn invalid_address() -> WalletError {
    WalletError::new(
        WalletErrorKind::InvalidAddress,
        "invalid canonical Solana address",
    )
}

impl TransactionFactory for Wallet {
    fn transaction(&self) -> Box<dyn base::TransactionBuilder> {
        Box::new(TransferBuilder::new(self))
    }

    fn resume(
        &self,
        snapshot: &TransactionSnapshot,
    ) -> Result<Box<dyn base::TransactionBuilder>, TransactionError> {
        Ok(Box::new(TransferBuilder::restore(self, snapshot)?))
    }

    fn broadcaster(&self) -> &dyn Broadcaster {
        self
    }
}

impl SingleSender for Wallet {
    /// One guarded operation through the outbox: the sender stays reserved
    /// from the account read until the submission settles.
    fn send<'a>(
        &'a self,
        destination: base::Address,
        amount: Decimal,
    ) -> FutureResult<'a, base::TransactionId> {
        Box::pin(async move {
            let to = Address::try_from(&destination).map_err(|_| invalid_address())?;
            let request = self.request(to, &amount)?;
            let accepted = self.outbox.send(vec![request]).await.map_err(|error| {
                let mut source = error.source;
                source.ambiguous_transaction_id = error.ambiguous_transaction_id;
                source
            })?;
            let [id] = <[_; 1]>::try_from(accepted).map_err(|_| {
                WalletError::new(
                    WalletErrorKind::Transaction,
                    "Solana single submission returned an invalid result count",
                )
            })?;
            Ok(id)
        })
    }
}

impl Broadcaster for Wallet {
    /// Submits a transaction this wallet's factory prepared, once.
    fn broadcast<'a>(
        &'a self,
        prepared: &'a base::SignedTransaction,
    ) -> TransactionFuture<'a, Result<Submission, TransactionError>> {
        Box::pin(async move {
            let id = TransferBuilder::decode(prepared)?;
            let id = self.outbox.broadcast(&id, prepared.envelope()).await?;
            Ok(Submission { id })
        })
    }
}

#[cfg(test)]
mod tests {
    use wallets::WalletFactory as _;

    use super::*;
    use crate::native::Seed;
    use crate::native::rpc::script::{Script, Scripted};
    use crate::sender::stub::{Node, factory, key};

    fn node_factory(node: &Node) -> WalletFactory {
        let client = Client::unverified(node.clone(), Script::network());
        factory(client, Arc::new(Spawner))
    }

    /// `address` in the fixture cluster.
    fn base58(address: &Address) -> base::Address {
        address.canonical(&Script::network().canonical())
    }

    #[test]
    fn config_is_native_sol_on_the_network_network() {
        let config = WalletConfig::new(&Script::network());
        assert_eq!(config.network(), &Script::network().canonical());
        assert_eq!(config.asset(), &AssetKind::Native);
        assert!(config.validate().is_ok());
        let foreign = WalletConfig {
            network: base::Network::new(base::ChainName::new("ethereum"), "localnet"),
            ..config
        };
        assert_eq!(
            foreign.validate().unwrap_err().kind,
            ErrorKind::InvalidConfiguration
        );
    }

    #[tokio::test]
    async fn insertion_validation_requires_the_network_and_the_same_outbox() {
        let node = Node::default();
        let factory = node_factory(&node);
        let wallet = factory.create(SecretBytes::new([7; 32])).await.unwrap();
        factory.validate(wallet.as_ref()).unwrap();
        let repeated = factory.create(SecretBytes::new([7; 32])).await.unwrap();
        factory.validate(wallet.as_ref()).unwrap();
        factory.validate(repeated.as_ref()).unwrap();
        let foreign = base::Network::new(CHAIN_NAME, "other");
        let other_network = WalletFactory {
            config: WalletConfig {
                network: foreign.clone(),
                ..factory.config.clone()
            },
            accounts: factory.accounts.clone(),
            outbox: factory.outbox.clone(),
        };
        let error = other_network.validate(wallet.as_ref()).unwrap_err();
        assert_eq!(error.kind, WalletErrorKind::Unsupported);
        let unrelated = node_factory(&node);
        let error = unrelated.validate(wallet.as_ref()).unwrap_err();
        assert_eq!(error.kind, WalletErrorKind::Unsupported);
    }

    #[tokio::test]
    async fn imported_generated_and_attached_wallets_share_canonical_capabilities() {
        let factory = node_factory(&Node::default());
        let imported = factory.create(SecretBytes::new([7; 32])).await.unwrap();
        let expected = key(7).address().clone();
        assert_eq!(imported.address(), &base58(&expected));
        let attached = factory
            .attach(KeyPair::import(SecretBytes::new([7; 32])).unwrap())
            .unwrap();
        assert_eq!(attached.address(), imported.address());
        let generated = factory.generate().await.unwrap();
        assert_ne!(generated.wallet.address(), imported.address());
        let rebuilt = factory.create(generated.material).await.unwrap();
        assert_eq!(rebuilt.address(), generated.wallet.address());
    }

    /// The one parse is `crypto::SecretBytes::from_hex` and the length is
    /// this factory's `create`. The native `Seed` keeps its stricter
    /// lowercase-only rule for callers driving `native` directly; the
    /// factory no longer imposes it, because a key is the same 32 bytes in
    /// either case and the other two chains always accepted both.
    #[tokio::test]
    async fn seed_bytes_are_exactly_32_from_hexadecimal_in_either_case() {
        let factory = node_factory(&Node::default());
        let accepted = "ab".repeat(32);
        for text in [accepted.clone(), accepted.to_uppercase()] {
            let seed = SecretBytes::from_hex(&text).expect("canonical Solana seed");
            assert_eq!(seed.as_bytes(), [0xab; 32]);
            factory.create(seed).await.expect("a 32-byte Ed25519 seed");
        }
        assert!(
            accepted.to_uppercase().parse::<Seed>().is_err(),
            "native Seed text stays lowercase-only"
        );
        for rejected in [
            format!("0x{accepted}"),
            format!(" {accepted}"),
            "z1".repeat(32),
        ] {
            assert!(SecretBytes::from_hex(&rejected).is_err(), "{rejected}");
        }
        for rejected in ["ab".repeat(31), format!("{accepted}00")] {
            let seed = SecretBytes::from_hex(&rejected).expect("hexadecimal, wrong length");
            let error = factory.create(seed).await.err().expect("wrong seed length");
            assert_eq!(error.kind, WalletErrorKind::InvalidSecret);
            assert_eq!(
                error.message,
                "secret key must be exactly 32 Ed25519 seed bytes"
            );
        }
    }

    #[tokio::test]
    async fn rejects_invalid_imports_and_keeps_generation_failures() {
        let factory = node_factory(&Node::default());
        let invalid = factory
            .create(SecretBytes::new([3; 31]))
            .await
            .err()
            .unwrap();
        assert_eq!(invalid.kind, WalletErrorKind::InvalidSecret);
        let failure = Generation::of(
            &factory,
            Err(Error::new(ErrorKind::KeyGeneration, "no randomness").into()),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(
            (failure.kind, failure.message.as_str()),
            (WalletErrorKind::Generation, "no randomness")
        );
    }

    #[test]
    fn parsed_address_is_canonical_base58_only() {
        let factory = node_factory(&Node::default());
        let address = key(31).address().clone();
        assert_eq!(
            factory.parse_address(&address.to_string()).unwrap(),
            base58(&address)
        );
        for text in [
            String::new(),
            format!("{address} "),
            format!("0x{address}"),
            "0".to_owned(),
        ] {
            let error = factory.parse_address(&text).unwrap_err();
            assert_eq!(error.kind, WalletErrorKind::InvalidAddress);
        }
    }

    /// The store-free parse and the factory's are one parse, not two that
    /// agree: the factory's impl delegates, and the configuration a caller
    /// builds from a `Network` alone is the same value the factory holds.
    #[test]
    fn the_factorys_parse_is_the_configurations_own() {
        let factory = node_factory(&Node::default());
        let config = WalletConfig::new(&Script::network());
        assert_eq!(config, factory.config);
        let address = key(31).address().clone();
        for text in [
            address.to_string(),
            format!("{address} "),
            format!("0x{address}"),
            String::new(),
            "0".to_owned(),
        ] {
            let reported = |error: WalletError| (error.kind, error.message);
            assert_eq!(
                config.parse_address(&text).map_err(reported),
                factory.parse_address(&text).map_err(reported),
            );
        }
    }

    /// The mint every token fixture below holds, with 6 decimals.
    fn mint() -> Address {
        Address::from_bytes([9; 32])
    }

    fn token_config() -> WalletConfig {
        WalletConfig::new(&Script::network()).with_token(mint(), 6)
    }

    #[test]
    fn config_selects_either_sol_or_one_mint_with_its_decimals() {
        let native = WalletConfig::new(&Script::network());
        assert_eq!(native.asset(), &AssetKind::Native);
        assert_eq!(native.decimals, 9);
        let token = token_config();
        assert_eq!(token.asset(), &AssetKind::SplToken(mint()));
        assert_eq!(token.decimals, 6);
        assert_eq!(token.network(), native.network());
        token.validate().expect("a mint with its own decimals");

        let mismatched = WalletConfig {
            decimals: 6,
            ..native.clone()
        };
        assert_eq!(
            mismatched.validate().unwrap_err().message,
            "Solana wallet asset and decimals must agree"
        );
        let absurd = WalletConfig {
            decimals: 256,
            ..token.clone()
        };
        assert!(absurd.validate().is_err());
    }

    #[test]
    fn a_token_factory_shares_the_native_send_pipeline_of_one_cluster() {
        let sol = node_factory(&Node::default());
        let token = sol
            .sibling(token_config())
            .expect("a sibling of one cluster");
        assert!(Arc::ptr_eq(&sol.outbox, &token.outbox));
        assert!(Arc::ptr_eq(&sol.accounts, &token.accounts));

        let foreign = WalletConfig {
            network: base::Network::new(CHAIN_NAME, "other"),
            ..token_config()
        };
        let error = sol.sibling(foreign).err().expect("another cluster");
        assert_eq!(
            error.message,
            "a Solana sibling factory must use the same cluster"
        );
        let unsound = WalletConfig {
            decimals: 6,
            ..WalletConfig::new(&Script::network())
        };
        let error = sol.sibling(unsound).err().expect("incoherent decimals");
        assert_eq!(error.kind, ErrorKind::InvalidConfiguration);
    }

    /// A factory states which asset its wallets hold, and nothing about what
    /// an amount of it is worth: that is the chain configuration's
    /// (`Config::units`).
    #[test]
    fn the_factory_asset_is_sol_or_the_configured_mint() {
        let sol = node_factory(&Node::default());

        assert_eq!(sol.asset(), base::AssetId::native(CHAIN_NAME));
        let token = sol.sibling(token_config()).unwrap();
        assert_eq!(token.asset(), AssetKind::SplToken(mint()).id());
        assert_eq!(token.asset().asset, mint().to_string());
    }

    /// The whole of what this integration will not do, declared once instead
    /// of refused four times. An SPL factory returns no `Authorship`, so
    /// `wallets::Wallets` has no author and no sender to hand out for its
    /// tag, its wallets are plain `wallets::Wallet`s with neither `send` nor
    /// `transaction` on them, and nothing reaches the node.
    #[tokio::test]
    async fn a_token_factory_declares_no_authorship_and_a_native_one_does() {
        let node = Node::default();
        let sol = node_factory(&node);
        let token = sol.sibling(token_config()).unwrap();
        let held = token.create(SecretBytes::new([7; 32])).await.unwrap();
        let to = base58(key(8).address());

        assert!(sol.authorship().is_some(), "native SOL is sent");
        assert!(
            token.authorship().is_none(),
            "an SPL mint is indexed and reported, and never sent"
        );

        let mut map = wallets::Wallets::new();
        map.add("usdc", token).unwrap();
        let Err(refused) = map.author(&"usdc", &held) else {
            panic!("a token asset must have no authoring capability")
        };
        let batch = map
            .send_all(
                &"usdc",
                vec![wallets::Transfer {
                    wallet: held,
                    to,
                    amount: "1".parse().unwrap(),
                }],
            )
            .await
            .expect_err("and no sender to reach");

        assert_eq!(refused.kind, WalletErrorKind::Unsupported);
        assert_eq!(batch.source.kind, WalletErrorKind::Unsupported);
        assert!(node.methods.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn single_send_validates_inputs_then_sends_once_through_the_outbox() {
        let node = Node::default();
        let factory = node_factory(&node);
        let wallet = factory.authored(SecretBytes::new([7; 32])).await;
        let to = key(8).address().clone();
        let id = wallet
            .send(base58(&to), "1.000000001".parse().unwrap())
            .await
            .expect("native send");
        assert!(id.as_str().parse::<crate::native::TransactionId>().is_ok());
        let sent = |node: &Node| {
            let methods = node.methods.lock().unwrap();
            methods
                .iter()
                .filter(|method| *method == "sendTransaction")
                .count()
        };
        assert_eq!(sent(&node), 1);
        for (address, amount) in [
            (base58(key(7).address()), Decimal::from(1_u64)),
            (base58(&to), Decimal::zero()),
            (
                base::Address::new(Script::network().canonical(), format!("0x{to}")),
                Decimal::from(1_u64),
            ),
        ] {
            assert!(wallet.send(address, amount).await.is_err());
        }
        assert_eq!(sent(&node), 1);
    }

    #[tokio::test]
    async fn single_send_moves_an_unknown_outcome_id_into_its_error() {
        let node = Node {
            lost: true,
            ..Node::default()
        };
        let factory = node_factory(&node);
        let wallet = factory.authored(SecretBytes::new([7; 32])).await;
        let error = wallet
            .send(base58(key(8).address()), Decimal::from(1_u64))
            .await
            .unwrap_err();
        assert_eq!(error.kind, WalletErrorKind::Unavailable);
        assert_eq!(error.message, "Solana submission outcome is unknown");
        assert!(error.ambiguous_transaction_id.is_some());
        let busy = wallet
            .send(base58(key(8).address()), Decimal::from(1_u64))
            .await
            .unwrap_err();
        assert_eq!(
            (busy.kind, busy.ambiguous_transaction_id),
            (WalletErrorKind::SourceBusy, None)
        );
    }
}
