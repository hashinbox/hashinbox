//! Ethereum wallets: [`WalletConfig`], the [`WalletFactory`] that makes
//! them (`wallets::WalletFactory`), and the wallet itself.

use std::sync::Arc;

use base::{
    Broadcaster, Decimal, Networked, SecretBytes, SignFuture, SignRequest, Signer, Submission,
    TransactionError, TransactionFuture, TransactionSnapshot,
};
use indexing::Holder;
use wallets::{
    AddressParser, Authorship, Dependency, FactoryProfile, FutureResult, Generation, Origin,
    SingleSender, TransactionFactory, Wallet as WalletContract, WalletError, WalletErrorKind,
};

use crate::TransferBuilder;
use crate::native::{
    Address, AssetKind, ETH, Error, ErrorKind, KeyPair, Network, Wei,
    rpc::{Accounts, Client},
    transaction::{FeeLimits, Transfer},
};
use crate::sender::{FactorySender, Outbox, SendRequest};

/// What one asset's wallets send and how much they may spend on fees.
///
/// [`WalletConfig::new`] selects ether on `network` with [`FeeLimits::default`];
/// `with_*` methods override one choice each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalletConfig {
    pub(crate) network: base::Network,
    /// The chain this wallet signs for; checked against the node before signing.
    pub(crate) chain_id: u64,
    pub(crate) asset: AssetKind,
    pub(crate) decimals: u32,
    pub(crate) fee_limits: FeeLimits,
}

impl WalletConfig {
    /// Ether on `network`, 18 decimals, default fee limits.
    #[must_use]
    pub fn new(network: &Network) -> Self {
        Self {
            network: network.canonical(),
            chain_id: network.chain_id,
            asset: AssetKind::Native,
            decimals: ETH.decimals,
            fee_limits: FeeLimits::default(),
        }
    }

    /// Sends the ERC-20 token at `contract`, which has `decimals` decimals.
    #[must_use]
    pub fn with_token(mut self, contract: Address, decimals: u8) -> Self {
        self.asset = AssetKind::Erc20(contract);
        self.decimals = decimals.into();
        self
    }

    /// Replaces the default [`FeeLimits`].
    #[must_use]
    pub fn with_fee_limits(mut self, fee_limits: FeeLimits) -> Self {
        self.fee_limits = fee_limits;
        self
    }

    /// The index network of every wallet with this configuration.
    #[must_use]
    pub const fn network(&self) -> &base::Network {
        &self.network
    }

    /// The asset wallets of this configuration send.
    #[must_use]
    pub const fn asset(&self) -> &AssetKind {
        &self.asset
    }

    pub(crate) fn validate(&self) -> Result<(), Error> {
        if self.network.chain != crate::CHAIN_NAME
            || self.network.name.trim().is_empty()
            || self.chain_id == 0
            || matches!(self.asset, AssetKind::Native) && self.decimals != ETH.decimals
            || matches!(&self.asset, AssetKind::Erc20(token) if token.is_zero())
            || self.decimals > u8::MAX.into()
        {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Ethereum wallet network, asset, and decimals must agree",
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
    /// Any-case `0x` hex; mixed case must carry a valid EIP-55 checksum.
    /// Returns the lowercase canonical text.
    ///
    /// What accepting `text` proves is the weakest of the three chains, and
    /// deliberately so: exactly twenty hexadecimal bytes behind `0x`, plus the
    /// EIP-55 checksum when the text is mixed case. Single-case text carries
    /// no checksum at all, so a typo in it is a different, equally valid
    /// address; the network is not in the text either, since every EVM chain
    /// shares one encoding, so it comes from this configuration.
    fn parse_address(&self, text: &str) -> Result<base::Address, WalletError> {
        Ok(text.parse::<Address>()?.canonical(&self.network))
    }
}

/// Makes Ethereum wallets of one [`WalletConfig`] over one RPC client:
/// `wallets::WalletFactory`, registered with `wallets::Wallets::register`.
///
/// It reads no index: the index answers both balance and history from the
/// movements it stores. The node reads this factory holds are the ones a send
/// needs (nonce, affordability).
pub struct WalletFactory {
    config: WalletConfig,
    accounts: Arc<dyn Accounts>,
    outbox: Arc<Outbox>,
}

impl WalletFactory {
    /// Builds balance reads and the send pipeline over `rpc`. No I/O.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] when `config` is inconsistent or
    /// `config` and `rpc` name different networks.
    pub fn new<C>(config: WalletConfig, rpc: &Client<C>) -> Result<Self, Error>
    where
        C: json_rpc::Client + 'static,
    {
        let network = rpc.network();
        if config.chain_id != network.chain_id || config.network != network.canonical() {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Ethereum wallet configuration and RPC client must share one network",
            ));
        }
        config.validate()?;
        let rpc = Arc::new(rpc.clone());
        let outbox = Outbox::new(config.chain_id, rpc.clone(), rpc.clone(), rpc.clone());
        Ok(Self::assemble(config, rpc, Arc::new(outbox)))
    }

    /// A factory for another asset of the same network that shares this
    /// one's RPC and send pipeline, so ETH and ERC-20 wallets of one address
    /// never reuse a nonce.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] for another network or an
    /// inconsistent `config`.
    pub fn sibling(&self, config: WalletConfig) -> Result<Self, Error> {
        if config.network != self.config.network || config.chain_id != self.config.chain_id {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "an Ethereum sibling factory must use the same network",
            ));
        }
        config.validate()?;
        Ok(Self::assemble(
            config,
            self.accounts.clone(),
            self.outbox.clone(),
        ))
    }

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

    /// Turns a local key into a wallet of this factory's asset.
    ///
    /// # Errors
    ///
    /// `Unsupported` when the configuration is inconsistent.
    pub fn attach(&self, key: KeyPair) -> Result<Arc<dyn WalletContract>, WalletError> {
        self.config.validate()?;
        Ok(Arc::new(Wallet {
            canonical: key.address().canonical(&self.config.network),
            asset: self.config.asset.id(),
            config: self.config.clone(),
            key: Arc::new(key),
            accounts: self.accounts.clone(),
            outbox: self.outbox.clone(),
        }))
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

    /// Ether, or the configured ERC-20 token's contract.
    fn asset(&self) -> indexing::AssetId {
        self.config.asset.id()
    }

    /// Every asset this chain carries is one it also spends, so every factory
    /// declares one.
    fn authorship(&self) -> Option<Arc<dyn Authorship>> {
        Some(Arc::new(FactorySender::new(
            self.config.clone(),
            self.outbox.clone(),
        )))
    }
}

/// One Ethereum address with its factory's capabilities attached.
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

    /// The send request that sends `amount` of the configured asset from this
    /// wallet to `to`.
    pub(crate) fn request(
        &self,
        to: Address,
        amount: &Decimal,
    ) -> Result<SendRequest, WalletError> {
        let amount = Wei::from_decimal(amount, self.config.decimals)
            .map_err(|error| WalletError::new(WalletErrorKind::InvalidAmount, error.message))?;
        let transfer = Transfer {
            asset: self.config.asset.clone(),
            to,
            amount,
        };
        Ok(SendRequest::new(
            self.key.clone(),
            self.key.address().clone(),
            transfer,
            self.config.fee_limits.clone(),
        ))
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
        self.key.sign(request)
    }
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
    /// from the nonce read until the submission settles.
    fn send<'a>(
        &'a self,
        destination: base::Address,
        amount: wallets::Amount,
    ) -> FutureResult<'a, base::TransactionId> {
        Box::pin(async move {
            let to = Address::try_from(&destination)?;
            let request = match amount {
                wallets::Amount::Exact(amount) => self.request(to, &amount)?,
                wallets::Amount::All => SendRequest::sweep(
                    self.key.clone(),
                    self.key.address().clone(),
                    to,
                    self.config.asset.clone(),
                    self.config.fee_limits.clone(),
                ),
            };
            let accepted = self.outbox.send(vec![request]).await.map_err(|error| {
                let mut source = error.source;
                source.ambiguous_transaction_id = error.ambiguous_transaction_id;
                source
            })?;
            let [id] = <[_; 1]>::try_from(accepted).map_err(|_| {
                WalletError::new(
                    WalletErrorKind::Transaction,
                    "Ethereum single submission returned an invalid result count",
                )
            })?;
            Ok(id)
        })
    }
}

impl Broadcaster for Wallet {
    fn broadcast<'a>(
        &'a self,
        prepared: &'a base::SignedTransaction,
    ) -> TransactionFuture<'a, Result<Submission, TransactionError>> {
        Box::pin(async move {
            let signed = TransferBuilder::decode(prepared)?;
            let id = self.outbox.broadcast(signed).await?;
            Ok(Submission { id: id.into() })
        })
    }
}
