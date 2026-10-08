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
/// checkpoint and its commit announcements. Whether a transaction landed is
/// the third thing it needs, and it is handed in as
/// [`crate::sender::Presence`], so this factory names no store at all.
pub struct WalletFactory {
    config: WalletConfig,
    accounts: Arc<dyn Accounts>,
    outbox: Arc<Outbox>,
}

impl WalletFactory {
    /// Builds balance reads and the send pipeline over `rpc`, and the
    /// recovery of unknown outcomes over `indexer` and `presence`. Submissions
    /// run on the current Tokio runtime; supply a supervising registrar with
    /// [`Self::with_registrar`]. No I/O.
    ///
    /// `presence` is the sender's absence proof and nothing else: whether the
    /// index holds a transaction at one stable checkpoint
    /// ([`crate::sender::IndexPresence`] over the store and this index).
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] when `config`, `rpc`, and
    /// `indexer` name different networks.
    pub fn new<C, R>(
        config: WalletConfig,
        rpc: &Client<C>,
        indexer: &Arc<Indexer<Source<C>, BlockInterpreter, R>>,
        presence: Arc<dyn crate::sender::Presence>,
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
            presence,
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

    fn key_format(&self) -> base::KeyFormat {
        base::KeyFormat::Ed25519Seed
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
        amount: wallets::Amount,
    ) -> FutureResult<'a, base::TransactionId> {
        Box::pin(async move {
            let to = Address::try_from(&destination).map_err(|_| invalid_address())?;
            let request = match amount {
                wallets::Amount::Exact(amount) => self.request(to, &amount)?,
                wallets::Amount::All => {
                    SendRequest::sweep(self.key.clone(), self.key.address().clone(), to)?
                }
            };
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
