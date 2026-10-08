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
    /// A wallet over `node` for every dependency, for adapter tests.
    #[cfg(test)]
    pub(crate) fn fixture(
        config: WalletConfig,
        key: KeyPair,
        node: Arc<crate::sender::stub::Node>,
    ) -> Self {
        Self {
            canonical: key.address().canonical(&config.network),
            asset: config.asset.id(),
            config,
            key: Arc::new(key),
            outbox: Arc::new(node.outbox()),
            accounts: node,
        }
    }

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

#[cfg(test)]
mod tests {
    use alloy_primitives::keccak256;
    use base::{
        BlockRef, BoxFuture, Digest, PublicKeyFormat, SignablePayload, SignatureEncoding,
        SignatureScheme,
    };
    use futures_executor::block_on;
    use indexing::{ChainName, IndexError};

    use wallets::WalletFactory as _;

    use super::*;
    use base::TransactionErrorKind;

    use crate::native::{
        SignedTransaction, TransactionId, Wei,
        rpc::{Submitter, View},
        transaction::{BoundTransfer, BuildContext, Preflight},
    };

    struct InactiveDependencies;

    impl Accounts for InactiveDependencies {
        fn balance<'a>(
            &'a self,
            _address: Address,
            _asset: &'a AssetKind,
            _at: View,
        ) -> BoxFuture<'a, Result<Wei, Error>> {
            Box::pin(async { unreachable!("wallet generation must not read account balances") })
        }

        fn nonce<'a>(&'a self, _address: Address) -> BoxFuture<'a, Result<u64, Error>> {
            Box::pin(async { unreachable!("wallet generation must not read account nonces") })
        }
    }

    impl Preflight for InactiveDependencies {
        fn context<'a>(
            &'a self,
            _from: &'a Address,
            _transfer: &'a Transfer,
            _nonce: u64,
            _limits: &'a FeeLimits,
        ) -> BoxFuture<'a, Result<BuildContext, Error>> {
            Box::pin(async { unreachable!("wallet generation must not build a transaction") })
        }
    }

    impl Submitter for InactiveDependencies {
        fn broadcast<'a>(
            &'a self,
            _transaction: SignedTransaction,
        ) -> BoxFuture<'a, Result<TransactionId, Error>> {
            Box::pin(async { unreachable!("wallet generation must not broadcast a transaction") })
        }

        fn known<'a>(
            &'a self,
            _transaction: &'a TransactionId,
        ) -> BoxFuture<'a, Result<bool, Error>> {
            Box::pin(async { unreachable!("wallet generation must not query a transaction") })
        }
    }

    impl indexing::Checkpoint for InactiveDependencies {
        fn checkpoint<'a>(
            &'a self,
            _network: &'a base::Network,
        ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
            panic!("startup wallet insertion must not read the checkpoint")
        }
    }

    fn config(asset: AssetKind, decimals: u32) -> WalletConfig {
        let mut config = WalletConfig::new(&Network::new(11_155_111, "sepolia"));
        config.asset = asset;
        config.decimals = decimals;
        config
    }

    fn factory() -> WalletFactory {
        let dependencies = Arc::new(InactiveDependencies);
        let outbox = Outbox::new(
            11_155_111,
            dependencies.clone(),
            dependencies.clone(),
            dependencies.clone(),
        );
        WalletFactory::assemble(
            config(AssetKind::Native, ETH.decimals),
            dependencies,
            Arc::new(outbox),
        )
    }

    fn key() -> KeyPair {
        KeyPair::import(SecretBytes::new([1_u8; 32])).unwrap()
    }

    fn sibling(factory: &WalletFactory, config: WalletConfig) -> WalletFactory {
        WalletFactory::assemble(config, factory.accounts.clone(), factory.outbox.clone())
    }

    fn digest_request() -> SignRequest {
        SignRequest {
            payload: SignablePayload::Digest(Digest {
                bytes: vec![0x42; 32],
            }),
            scheme: SignatureScheme::EcdsaSecp256k1,
            encoding: SignatureEncoding::Recoverable,
            public_key_format: PublicKeyFormat::Raw,
            key_tweak: None,
        }
    }

    #[test]
    fn config_defaults_to_ether_and_overrides_one_choice_each() {
        let network = Network::new(1, "mainnet");
        let config = WalletConfig::new(&network);
        assert_eq!(config.network(), &network.canonical());
        assert_eq!(config.asset(), &AssetKind::Native);
        assert_eq!((config.chain_id, config.decimals), (1, 18));
        assert_eq!(config.fee_limits, FeeLimits::default());
        let token = config.clone().with_token(Address::from_bytes([7; 20]), 6);
        assert_eq!(
            (token.asset(), token.decimals),
            (&AssetKind::Erc20(Address::from_bytes([7; 20])), 6)
        );
        assert!(token.validate().is_ok());
        assert!(config.with_token(Address::ZERO, 6).validate().is_err());
    }

    #[test]
    fn factory_validation_accepts_equivalent_composition_without_chain_io() {
        let factory = factory();
        let wallet = factory.attach(key()).unwrap();
        let equivalent = sibling(&factory, factory.config.clone());
        assert!(factory.validate(wallet.as_ref()).is_ok());
        assert!(equivalent.validate(wallet.as_ref()).is_ok());
    }

    #[test]
    fn factory_validation_rejects_asset_chain_and_precision_mismatches_without_io() {
        let mut factory = factory();
        factory.config = config(AssetKind::Erc20(Address::from_bytes([0x33; 20])), 6);
        let wallet = factory.attach(key()).unwrap();
        for config in [
            config(AssetKind::Native, 18),
            config(AssetKind::Erc20(Address::from_bytes([0x44; 20])), 6),
            WalletConfig {
                chain_id: 1,
                ..factory.config.clone()
            },
            WalletConfig {
                decimals: 18,
                ..factory.config.clone()
            },
            WalletConfig {
                network: base::Network::new(crate::CHAIN_NAME, "mainnet"),
                ..factory.config.clone()
            },
            factory.config.clone().with_fee_limits(
                FeeLimits::new(1, 0, 1, Wei::from_u128(1), Wei::ZERO, Wei::from_u128(1)).unwrap(),
            ),
        ] {
            let incompatible = sibling(&factory, config);
            let error = incompatible.validate(wallet.as_ref()).unwrap_err();
            assert_eq!(error.kind, WalletErrorKind::Unsupported);
        }
    }

    #[test]
    fn factory_validation_rejects_unshared_operational_dependencies_without_io() {
        let factory = factory();
        let wallet = factory.attach(key()).unwrap();
        let other = Arc::new(InactiveDependencies);
        let other_outbox = Arc::new(Outbox::new(
            11_155_111,
            other.clone(),
            other.clone(),
            other.clone(),
        ));
        let config = factory.config.clone();
        for incompatible in [
            WalletFactory::assemble(config.clone(), other, factory.outbox.clone()),
            WalletFactory::assemble(config.clone(), factory.accounts.clone(), other_outbox),
        ] {
            let error = incompatible.validate(wallet.as_ref()).unwrap_err();
            assert_eq!(error.kind, WalletErrorKind::Unsupported);
        }
    }

    #[test]
    fn sibling_shares_dependencies_only_within_one_network() {
        let native = factory();
        let token = native
            .sibling(
                native
                    .config
                    .clone()
                    .with_token(Address::from_bytes([0x33; 20]), 6),
            )
            .unwrap();
        assert!(Arc::ptr_eq(&native.outbox, &token.outbox));
        let error = native
            .sibling(config(AssetKind::Native, 18).with_token(Address::ZERO, 6))
            .err()
            .unwrap();
        assert_eq!(error.kind, ErrorKind::InvalidConfiguration);
        let other = WalletConfig::new(&Network::new(1, "mainnet"));
        assert_eq!(
            native.sibling(other).err().unwrap().kind,
            ErrorKind::InvalidConfiguration
        );
    }

    /// ETH and USDC wallets of one key share an address, so only the factory
    /// can tell them apart — which is exactly what acceptance asks.
    #[test]
    fn the_collection_rejects_eth_and_usdc_mixups() {
        let native = factory();
        let token = native
            .sibling(
                native
                    .config
                    .clone()
                    .with_token(Address::from_bytes([0x33; 20]), 6),
            )
            .unwrap();
        let eth = native.attach(key()).unwrap();
        let usdc = token.attach(key()).unwrap();
        assert_eq!(eth.address(), usdc.address());
        let mut wallets = wallets::Wallets::<&str>::new();
        wallets.add("eth", native).unwrap();
        wallets.add("usdc", token).unwrap();

        wallets.accepts(&"eth", eth.as_ref()).unwrap();
        wallets.accepts(&"usdc", usdc.as_ref()).unwrap();
        for (asset, wallet) in [("usdc", &eth), ("eth", &usdc)] {
            let error = wallets.accepts(&asset, wallet.as_ref()).unwrap_err();
            assert_eq!(error.kind, WalletErrorKind::Unsupported);
        }
    }

    /// A factory states which asset its wallets hold, and nothing about what
    /// an amount of it is worth: that is the chain configuration's
    /// (`Config::units`).
    #[test]
    fn the_factory_asset_is_ether_or_the_configured_contract() {
        let native = factory();

        assert_eq!(native.asset(), AssetKind::Native.id());
        let contract = Address::from_bytes([0x33; 20]);
        let token = native
            .sibling(native.config.clone().with_token(contract.clone(), 6))
            .unwrap();
        assert_eq!(token.asset(), AssetKind::Erc20(contract).id());
    }

    #[test]
    fn attachment_preserves_local_identity_and_signatures_without_chain_io() {
        let key = key();
        let factory = factory();
        let address = key.address().canonical(&factory.config.network);
        let signature = block_on(Signer::sign(&key, digest_request())).unwrap();
        let wallet = factory.attach(key).unwrap();
        assert_eq!(wallet.address(), &address);
        assert_eq!(address.value, "0x1a642f0e3c3af545e7acbd38b07251b3990914f1");
        assert_eq!(block_on(wallet.sign(digest_request())).unwrap(), signature);
    }

    #[test]
    fn factory_parses_user_text_into_the_canonical_address_of_its_network() {
        let factory = factory();
        let canonical = "0x5aaeb6053f3e94c9b9a09f33669435e7ef1beaed";
        for text in [
            canonical,
            "0x5AAEB6053F3E94C9B9A09F33669435E7EF1BEAED",
            "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
        ] {
            let address = factory.parse_address(text).unwrap();
            assert_eq!(address.value, canonical);
            assert_eq!(address.network, factory.config.network);
        }
        for text in [
            "",
            "5aaeb6053f3e94c9b9a09f33669435e7ef1beaed",
            "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAeD",
        ] {
            let error = factory.parse_address(text).unwrap_err();
            assert_eq!(error.kind, WalletErrorKind::InvalidAddress);
        }
    }

    /// The store-free parse and the factory's are one parse, not two that
    /// agree: the factory's impl delegates, and the configuration a caller
    /// builds from a `Network` alone is the same value the factory holds.
    #[test]
    fn the_factorys_parse_is_the_configurations_own() {
        let factory = factory();
        let config = WalletConfig::new(&Network::new(11_155_111, "sepolia"));
        assert_eq!(config, factory.config);
        for text in [
            "0x5aaeb6053f3e94c9b9a09f33669435e7ef1beaed",
            "0x5AAEB6053F3E94C9B9A09F33669435E7EF1BEAED",
            "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
            "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAeD",
            "",
            "0xab",
        ] {
            let reported = |error: WalletError| (error.kind, error.message);
            assert_eq!(
                config.parse_address(text).map_err(reported),
                factory.parse_address(text).map_err(reported),
            );
        }
    }

    #[test]
    fn attachment_rejects_invalid_operational_configuration() {
        let mut factory = factory();
        factory.config.chain_id = 0;
        let error = factory.attach(key()).err().unwrap();
        assert_eq!(error.kind, WalletErrorKind::Unsupported);
    }

    #[test]
    fn attachment_keeps_eoa_identity_independent_of_network_and_asset() {
        let mut factory = factory();
        let native = factory.attach(key()).unwrap();
        factory.config = WalletConfig::new(&Network::new(31_337, "anvil"))
            .with_token(Address::from_bytes([0x33; 20]), 6);
        let token = factory.attach(key()).unwrap();
        assert_eq!(native.address().value, token.address().value);
    }

    #[test]
    fn creation_validates_configuration_before_secret_and_preserves_secret_errors() {
        let mut factory = factory();
        factory.config.chain_id = 0;
        let error = block_on(factory.create(SecretBytes::new(Vec::new())))
            .err()
            .expect("invalid configuration");
        assert_eq!(
            error,
            WalletError::new(
                WalletErrorKind::Unsupported,
                "Ethereum wallet network, asset, and decimals must agree",
            )
        );
        factory.config.chain_id = 1;
        for secret in [Vec::new(), vec![0; 32], vec![255; 32]] {
            let error = block_on(factory.create(SecretBytes::new(secret)))
                .err()
                .expect("invalid scalar");
            assert_eq!(
                error,
                WalletError::new(
                    WalletErrorKind::InvalidSecret,
                    "secret key must be a valid 32-byte secp256k1 scalar",
                )
            );
        }
    }

    #[test]
    fn generation_derives_address_from_native_secp256k1_signer() {
        let wallet = block_on(factory().generate()).unwrap().wallet;
        let signed = block_on(wallet.sign(digest_request())).unwrap();
        let hash = keccak256(&signed.public_key.bytes);
        let derived = Address::from_bytes(hash[12..].try_into().unwrap());
        assert_eq!(signed.public_key.curve, crypto::Curve::Secp256k1);
        assert_eq!(signed.public_key.format, PublicKeyFormat::Raw);
        assert_eq!(wallet.address().value, derived.to_string());
    }

    /// The one parse is `crypto::SecretBytes::from_hex`; the length and the
    /// curve are this factory's `create`. Together they reject exactly what
    /// the deleted `SecretParser` rejected.
    #[test]
    fn secret_text_is_hexadecimal_in_either_case_without_disclosure() {
        let factory = factory();
        for text in ["ab".repeat(32), "AB".repeat(32)] {
            let secret = SecretBytes::from_hex(&text).expect("hex secret");
            assert_eq!(secret.as_bytes(), [0xab; 32]);
            block_on(factory.create(secret)).expect("a 32-byte secp256k1 scalar");
        }
        for text in [format!("0x{}", "ab".repeat(32)), "zz".repeat(32)] {
            assert!(SecretBytes::from_hex(&text).is_err(), "{text}");
        }
        for text in ["ab".repeat(31), "ab".repeat(33)] {
            let secret = SecretBytes::from_hex(&text).expect("hexadecimal, wrong length");
            let error = block_on(factory.create(secret))
                .err()
                .expect("not a secp256k1 scalar");
            assert_eq!(error.kind, WalletErrorKind::InvalidSecret);
            assert!(!error.message.contains(&text));
        }
    }

    #[test]
    fn generated_key_reuses_create_and_preserves_signed_wire() {
        let factory = factory();
        let generated = block_on(Generation::of(&factory, Ok(SecretBytes::new([1_u8; 32]))));
        let generated = generated.unwrap();
        assert_eq!(generated.material.as_bytes(), [1_u8; 32]);
        let generated = generated.wallet;
        let created = block_on(factory.create(SecretBytes::new([1_u8; 32]))).unwrap();
        let local = key();
        let transaction = BoundTransfer::new(
            Transfer {
                asset: AssetKind::Native,
                to: Address::from_bytes([0x22; 20]),
                amount: Wei::from_u128(7),
            },
            BuildContext {
                chain_id: 11_155_111,
                from: Address::try_from(generated.address()).unwrap(),
                nonce: 2,
                gas_limit: 21_000,
                max_fee_per_gas: Wei::from_u128(10),
                max_priority_fee_per_gas: Wei::from_u128(3),
            },
        );
        let generated_signed = block_on(transaction.sign(generated.as_ref())).unwrap();
        let created_signed = block_on(transaction.sign(created.as_ref())).unwrap();
        let local_signed = block_on(transaction.sign(&local)).unwrap();
        let attached = factory.attach(local).unwrap();
        let attached_signed = block_on(transaction.sign(attached.as_ref())).unwrap();
        let fees = generated_signed.inspect_eip1559_fees().unwrap();
        assert_eq!(generated.address(), created.address());
        assert_eq!(generated_signed, created_signed);
        assert_eq!(generated_signed, local_signed);
        assert_eq!(generated_signed, attached_signed);
        let bytes = generated_signed.envelope.as_bytes();
        assert_eq!(bytes.first(), Some(&0x02));
        assert_eq!(*generated_signed.id.as_bytes(), keccak256(bytes).0);
        assert_eq!((fees.chain_id, fees.gas_limit), (11_155_111, 21_000));
    }

    #[test]
    fn generation_failure_is_typed_before_wallet_creation() {
        let mut factory = factory();
        factory.config.network.chain = ChainName::new("not-ethereum");
        let result = block_on(Generation::of(
            &factory,
            Err(Error::new(
                ErrorKind::KeyGeneration,
                "operating system random source is unavailable",
            )
            .into()),
        ));
        let Err(error) = result else {
            panic!("generation failure must not create a wallet");
        };
        assert_eq!(error.kind, WalletErrorKind::Generation);
        assert_eq!(
            error.message,
            "operating system random source is unavailable"
        );
    }

    #[test]
    fn builder_snapshots_preserve_native_and_token_asset_identity() {
        for (asset, decimals, expected) in [
            (
                AssetKind::Native,
                18,
                serde_json::json!({"kind": "native", "ticker": "ETH", "decimals": 18}),
            ),
            (
                AssetKind::Erc20(Address::from_bytes([0xab; 20])),
                6,
                serde_json::json!({
                    "kind": "erc20",
                    "token": "0xabababababababababababababababababababab",
                    "decimals": 6,
                }),
            ),
        ] {
            let mut factory = factory();
            factory.config = config(asset, decimals);
            let wallet = Wallet {
                canonical: key().address().canonical(&factory.config.network),
                asset: factory.config.asset.id(),
                config: factory.config.clone(),
                key: Arc::new(key()),
                accounts: factory.accounts.clone(),
                outbox: factory.outbox.clone(),
            };
            let mut builder = wallet.transaction();
            builder
                .transfer(
                    Address::from_bytes([0x22; 20]).canonical(&factory.config.network),
                    "1.25".parse().unwrap(),
                )
                .unwrap();
            let draft = builder.draft().unwrap();
            assert_eq!(draft.asset, factory.config.asset.id());
            assert_eq!(draft.source, *wallet.address());
            assert_eq!(draft.transfers.len(), 1);
            let snapshot = draft.snapshot;
            assert_eq!(snapshot.value()["asset"], expected);
            let restored = wallet.resume(&snapshot).expect("snapshot must restore");
            assert_eq!(restored.draft().unwrap().snapshot, snapshot);
        }
    }

    #[test]
    fn broadcast_rejects_foreign_or_tampered_prepared_transactions() {
        let factory = factory();
        let wallet = Wallet {
            canonical: key().address().canonical(&factory.config.network),
            asset: factory.config.asset.id(),
            config: factory.config.clone(),
            key: Arc::new(key()),
            accounts: factory.accounts.clone(),
            outbox: factory.outbox.clone(),
        };
        let bytes = vec![2, 1, 2];
        let id = base::TransactionId::from(TransactionId::from_bytes(keccak256(&bytes).0));
        for (kind, id) in [
            ("bitcoin.signed", id.clone()),
            (
                TransferBuilder::PREPARED_KIND,
                base::TransactionId::new("0x00"),
            ),
            (
                TransferBuilder::PREPARED_KIND,
                base::TransactionId::from(TransactionId::from_bytes([0; 32])),
            ),
        ] {
            let prepared = base::SignedTransaction::new(
                kind,
                id,
                base::transaction::Envelope::new(bytes.clone()),
                Decimal::zero(),
            );
            let error = block_on(wallet.broadcaster().broadcast(&prepared)).unwrap_err();
            assert_eq!(error.kind, TransactionErrorKind::InvalidTransaction);
        }
    }
}
