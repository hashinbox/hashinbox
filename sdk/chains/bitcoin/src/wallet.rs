//! Bitcoin wallets: [`WalletConfig`], the [`WalletFactory`] that makes them
//! (`wallets::WalletFactory`), and the wallet itself.

use std::sync::Arc;

use base::{
    Broadcaster, Decimal, Networked, SecretBytes, SignFuture, SignRequest, Signer, Submission,
    TransactionError, TransactionFuture, TransactionSnapshot,
};
use bitcoin::AddressType as Kind;
use indexing::Holder;
use wallets::{
    AddressParser, Authorship, Dependency, FactoryProfile, FutureResult, Generation, Origin,
    SingleSender, TransactionFactory, Wallet as WalletContract, WalletError, WalletErrorKind,
};

use crate::TransferBuilder;
use crate::native::{
    Address, AddressType, AssetKind, CHAIN_NAME, Error, ErrorKind, KeyPair, Network, Satoshi,
    rpc::{Client, Submitter},
    transaction::{FeeRate, Preflight, Transfer},
};
use crate::sender::{FactorySender, Outbox, SendRequest, UtxoSource};

/// Which addresses one asset's wallets use and how they price fees.
///
/// [`WalletConfig::new`] selects P2WPKH on `network`, a
/// [`WalletConfig::FEE_TARGET_BLOCKS`]-block fee estimate, and Bitcoin Core's
/// default fee ceiling [`FeeRate::DEFAULT_MAXIMUM`] (0.1 BTC/kvB); `with_*`
/// methods override one choice each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalletConfig {
    pub(crate) network: base::Network,
    /// The protocol network `network` is the canonical identity of; it selects
    /// address encoding and key version bytes.
    pub(crate) native: Network,
    pub(crate) address_type: AddressType,
    pub(crate) fee_target_blocks: u16,
    pub(crate) max_fee_rate: FeeRate,
}

impl WalletConfig {
    /// Confirmation target of the default fee estimate, in blocks.
    pub const FEE_TARGET_BLOCKS: u16 = 6;

    /// P2WPKH on `network`, a six-block fee estimate, Core's default fee ceiling.
    #[must_use]
    pub fn new(network: &Network) -> Self {
        Self {
            network: network.canonical(),
            native: *network,
            address_type: AddressType::SegwitV0,
            fee_target_blocks: Self::FEE_TARGET_BLOCKS,
            max_fee_rate: FeeRate::DEFAULT_MAXIMUM,
        }
    }

    /// Derives wallet addresses of `address_type`.
    #[must_use]
    pub const fn with_address_type(mut self, address_type: AddressType) -> Self {
        self.address_type = address_type;
        self
    }

    /// Estimates fees for confirmation within `blocks` blocks.
    #[must_use]
    pub const fn with_fee_target_blocks(mut self, blocks: u16) -> Self {
        self.fee_target_blocks = blocks;
        self
    }

    /// Refuses to build or submit above `max_fee_rate`.
    #[must_use]
    pub const fn with_max_fee_rate(mut self, max_fee_rate: FeeRate) -> Self {
        self.max_fee_rate = max_fee_rate;
        self
    }

    /// The index network of every wallet with this configuration.
    #[must_use]
    pub const fn network(&self) -> &base::Network {
        &self.network
    }

    /// `to` as a native destination; it must belong to this configuration's
    /// network, since testnets share address prefixes.
    pub(crate) fn destination(&self, to: &base::Address) -> Result<Address, Error> {
        if !to.belongs_to(&self.network) {
            return Err(Error::new(
                ErrorKind::InvalidAddress,
                "Bitcoin destination belongs to another network",
            ));
        }
        Address::try_from(to)
    }

    pub(crate) fn validate(&self) -> Result<(), Error> {
        if self.network != self.native.canonical()
            || self.network.chain != CHAIN_NAME
            || self.fee_target_blocks == 0
            || self.max_fee_rate.sat_per_kvb() == 0
            || self.max_fee_rate > FeeRate::CORE_LIMIT
        {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Bitcoin wallet network, fee target, and fee ceiling must be valid",
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
    /// Any-case text of a supported address type of the configured network
    /// (testnet, signet, and testnet4 share prefixes, so the network is this
    /// configuration's own). Returns the canonical text.
    ///
    /// What accepting `text` proves: it is a Bitcoin address encoding — Base58
    /// with its check bytes, or Bech32/Bech32m with its checksum — whose
    /// version byte or witness HRP names this configuration's network, and
    /// whose script kind is one this crate can pay. It proves nothing about
    /// whether the address exists on chain, is funded, or has a key behind it.
    fn parse_address(&self, text: &str) -> Result<base::Address, WalletError> {
        let address = Address::parse(text, self.native)?;
        match address.kind(self.native)? {
            Some(
                Kind::P2pkh | Kind::P2sh | Kind::P2tr | Kind::P2a | Kind::P2wpkh | Kind::P2wsh,
            ) => Ok(address.canonical(&self.network)),
            None | Some(_) => Err(WalletError::new(
                WalletErrorKind::InvalidAddress,
                "Bitcoin address type is unsupported",
            )),
        }
    }
}

/// Makes Bitcoin wallets of one [`WalletConfig`] over one RPC client and the
/// index's spendable outputs: `wallets::WalletFactory`, registered with
/// `wallets::Wallets::register`.
///
/// The only index read it takes is that output set, for coin selection;
/// balances and history are served by `indexing` and `wallets::Wallets` from
/// the index itself.
pub struct WalletFactory {
    config: WalletConfig,
    outbox: Arc<Outbox>,
    source: Arc<dyn UtxoSource>,
}

impl WalletFactory {
    /// Builds the send pipeline over `rpc` and coin selection over the
    /// candidate inputs `source` supplies: caller-selected UTXOs (a
    /// `Vec<Utxo>`), or [`crate::sender::Spendable`] for the store's live
    /// outputs with the pending blocks folded in. No I/O.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] when `config` is invalid or
    /// `config` and `rpc` name different networks.
    pub fn new<C>(
        config: WalletConfig,
        rpc: &Client<C>,
        source: Arc<dyn UtxoSource>,
    ) -> Result<Self, Error>
    where
        C: json_rpc::Client + 'static,
    {
        if config.native != *rpc.network() {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Bitcoin wallet configuration and RPC client must share one network",
            ));
        }
        config.validate()?;
        let rpc = Arc::new(rpc.clone());
        Ok(Self::assemble(config, rpc.clone(), rpc, source))
    }

    pub(crate) fn assemble(
        config: WalletConfig,
        preflight: Arc<dyn Preflight>,
        submitter: Arc<dyn Submitter>,
        source: Arc<dyn UtxoSource>,
    ) -> Self {
        let outbox = Outbox::new(&config, preflight, submitter, source.clone());
        Self {
            config,
            outbox: Arc::new(outbox),
            source,
        }
    }

    /// Turns a local key into a wallet of this factory: the configuration
    /// chooses the network and address type.
    ///
    /// # Errors
    ///
    /// `Unsupported` when the configuration is invalid.
    pub fn attach(&self, key: KeyPair) -> Result<Arc<dyn WalletContract>, WalletError> {
        self.config.validate()?;
        let address = key.address(self.config.native, self.config.address_type);
        Ok(Arc::new(Wallet {
            canonical: address.canonical(&self.config.network),
            asset: indexing::AssetId::native(CHAIN_NAME),
            address,
            config: self.config.clone(),
            key: Arc::new(key),
            outbox: self.outbox.clone(),
            source: self.source.clone(),
        }))
    }

    fn origin(&self) -> Origin<Fingerprint> {
        Origin::new(Fingerprint {
            config: self.config.clone(),
            outbox: Dependency::of(&self.outbox),
            source: Dependency::of(&self.source),
        })
    }
}

/// What makes two wallets interchangeable: configuration and shared handles.
///
/// The handles are the whole send path: `outbox` is the guarded submission
/// pipeline, `source` the candidate inputs it selects from. Both are allocated once per
/// [`WalletFactory::assemble`], so two factories share them only by sharing
/// the assembly — which is what interchangeability means here.
#[derive(PartialEq, Eq)]
struct Fingerprint {
    config: WalletConfig,
    outbox: Dependency,
    source: Dependency,
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

    /// Native BTC: Bitcoin has one asset.
    fn asset(&self) -> indexing::AssetId {
        AssetKind::Native.id()
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

/// One Bitcoin address with its factory's capabilities attached.
#[derive(Clone)]
pub(crate) struct Wallet {
    pub(crate) config: WalletConfig,
    pub(crate) key: Arc<KeyPair>,
    /// Derived from `key` at attachment, for the configured network and type.
    pub(crate) address: Address,
    /// `address` in the configured network, computed once.
    canonical: base::Address,
    /// BTC: the one asset a Bitcoin wallet holds, computed once.
    asset: indexing::AssetId,
    pub(crate) outbox: Arc<Outbox>,
    /// The candidate inputs this wallet may spend, through `outbox`.
    source: Arc<dyn UtxoSource>,
}

impl Wallet {

    fn fingerprint(&self) -> Fingerprint {
        Fingerprint {
            config: self.config.clone(),
            outbox: Dependency::of(&self.outbox),
            source: Dependency::of(&self.source),
        }
    }

    /// The send request that pays `amount` BTC from this wallet to `to`.
    pub(crate) fn request(
        &self,
        to: Address,
        amount: &Decimal,
    ) -> Result<SendRequest, WalletError> {
        let amount = Satoshi::from_decimal(amount)
            .map_err(|error| WalletError::new(WalletErrorKind::InvalidAmount, error.message))?;
        Ok(SendRequest::new(
            self.key.clone(),
            self.address.clone(),
            Transfer::new(to, amount),
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
    /// One guarded operation through the outbox: the selected outputs stay
    /// reserved from selection until their spend is indexed or refused.
    fn send<'a>(
        &'a self,
        destination: base::Address,
        amount: wallets::Amount,
    ) -> FutureResult<'a, base::TransactionId> {
        Box::pin(async move {
            let to = self.config.destination(&destination)?;
            let request = match amount {
                wallets::Amount::Exact(amount) => self.request(to, &amount)?,
                wallets::Amount::All => {
                    SendRequest::sweep(self.key.clone(), self.address.clone(), to)
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
                    "Bitcoin single submission returned an invalid result count",
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
