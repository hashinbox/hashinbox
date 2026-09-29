//! Bitcoin wallets: [`WalletConfig`], the [`WalletFactory`] that makes them
//! (`wallets::WalletFactory`), and the wallet itself.

use std::sync::Arc;

use base::{
    Broadcaster, Decimal, Networked, SecretBytes, SignFuture, SignRequest, Signer, Submission,
    TransactionError, TransactionFuture, TransactionSnapshot,
};
use bitcoin::AddressType as Kind;
use indexing::Holder;
use indexing::OutputStore;
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
use crate::sender::{Batch, Item, Outbox, UtxoReader};

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
    utxos: Arc<UtxoReader>,
}

impl WalletFactory {
    /// Builds the send pipeline over `rpc` and coin selection over
    /// `spendable`, which is the reader the chain's [`crate::Indexer`] hands out
    /// (`indexing::Service::outputs`): the store's rows with this index's
    /// pending overlay folded in, so a wallet can spend its own change at depth
    /// one. No I/O.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] when `config` is invalid or
    /// `config` and `rpc` name different networks.
    pub fn new<C>(
        config: WalletConfig,
        rpc: &Client<C>,
        spendable: Arc<dyn OutputStore>,
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
        Ok(Self::assemble(config, rpc.clone(), rpc, spendable))
    }

    pub(crate) fn assemble(
        config: WalletConfig,
        preflight: Arc<dyn Preflight>,
        submitter: Arc<dyn Submitter>,
        spendable: Arc<dyn OutputStore>,
    ) -> Self {
        let utxos = Arc::new(UtxoReader::new(config.native, spendable));
        let outbox = Outbox::new(&config, preflight, submitter, utxos.clone());
        Self {
            config,
            outbox: Arc::new(outbox),
            utxos,
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
            utxos: self.utxos.clone(),
        }))
    }

    fn origin(&self) -> Origin<Fingerprint> {
        Origin::new(Fingerprint {
            config: self.config.clone(),
            outbox: Dependency::of(&self.outbox),
            utxos: Dependency::of(&self.utxos),
        })
    }
}

/// What makes two wallets interchangeable: configuration and shared handles.
///
/// The handles are the whole send path: `outbox` is the guarded submission
/// pipeline, `utxos` the coin selection it reads. Both are allocated once per
/// [`WalletFactory::assemble`], so two factories share them only by sharing
/// the assembly — which is what interchangeability means here.
#[derive(PartialEq, Eq)]
struct Fingerprint {
    config: WalletConfig,
    outbox: Dependency,
    utxos: Dependency,
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
        Some(Arc::new(Batch::new(
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
    /// Confirmed outputs with the pending overlay folded in: what this wallet
    /// may spend, through `outbox`.
    utxos: Arc<UtxoReader>,
}

impl Wallet {
    /// A wallet over `node` for every dependency, for adapter tests.
    #[cfg(test)]
    pub(crate) fn fixture(
        config: WalletConfig,
        key: KeyPair,
        address: Address,
        node: &Arc<crate::sender::stub::Node>,
    ) -> Self {
        let utxos = Arc::new(UtxoReader::new(config.native, node.clone()));
        let outbox = Outbox::new(&config, node.clone(), node.clone(), utxos.clone());
        Self {
            canonical: address.canonical(&config.network),
            asset: indexing::AssetId::native(CHAIN_NAME),
            config,
            key: Arc::new(key),
            address,
            outbox: Arc::new(outbox),
            utxos,
        }
    }

    fn fingerprint(&self) -> Fingerprint {
        Fingerprint {
            config: self.config.clone(),
            outbox: Dependency::of(&self.outbox),
            utxos: Dependency::of(&self.utxos),
        }
    }

    /// The outbox item that pays `amount` BTC from this wallet to `to`.
    pub(crate) fn item(&self, to: Address, amount: &Decimal) -> Result<Item, WalletError> {
        let amount = Satoshi::from_decimal(amount)
            .map_err(|error| WalletError::new(WalletErrorKind::InvalidAmount, error.message))?;
        Ok(Item::new(
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
        amount: Decimal,
    ) -> FutureResult<'a, base::TransactionId> {
        Box::pin(async move {
            let to = self.config.destination(&destination)?;
            let item = self.item(to, &amount)?;
            let accepted = self.outbox.send(vec![item]).await.map_err(|error| {
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

#[cfg(test)]
mod tests {
    use base::{Digest, PublicKeyFormat, SignablePayload, SignatureEncoding, SignatureScheme};
    use futures_executor::block_on;
    use indexing::{AssetId, ChainName};
    use wallets::WalletFactory as _;

    use base::TransactionErrorKind;

    use super::*;
    use crate::native::transaction::FeeRate;
    use crate::sender::stub::Node;

    fn factory() -> (Arc<Node>, WalletFactory) {
        let node = Arc::new(Node::default());
        let factory = node.factory();
        (node, factory)
    }

    fn equivalent(factory: &WalletFactory) -> WalletFactory {
        WalletFactory {
            config: factory.config.clone(),
            outbox: factory.outbox.clone(),
            utxos: factory.utxos.clone(),
        }
    }

    #[test]
    fn config_defaults_use_segwit_six_blocks_and_cores_fee_ceiling() {
        let config = WalletConfig::new(&Network::Regtest);
        assert_eq!(config.network(), &Network::Regtest.canonical());
        assert_eq!(config.address_type, AddressType::SegwitV0);
        assert_eq!(config.fee_target_blocks, 6);
        assert_eq!(config.max_fee_rate, FeeRate::from_sat_per_vb(10_000));
        assert!(config.validate().is_ok());
        for invalid in [
            config.clone().with_fee_target_blocks(0),
            config
                .clone()
                .with_max_fee_rate(FeeRate::from_sat_per_kvb(0)),
            config
                .clone()
                .with_max_fee_rate(FeeRate::from_sat_per_kvb(100_000_001)),
            WalletConfig {
                network: Network::Mainnet.canonical(),
                ..config.clone()
            },
        ] {
            assert_eq!(
                invalid.validate().unwrap_err().kind,
                ErrorKind::InvalidConfiguration
            );
        }
    }

    #[test]
    fn validation_accepts_equivalent_factories_and_rejects_other_networks_without_io() {
        let (node, factory) = factory();
        let wallet = factory.attach(Node::key(1)).unwrap();
        assert!(factory.validate(wallet.as_ref()).is_ok());
        assert!(equivalent(&factory).validate(wallet.as_ref()).is_ok());
        let mainnet = WalletFactory {
            config: WalletConfig::new(&Network::Mainnet),
            ..equivalent(&factory)
        };
        let error = mainnet.validate(wallet.as_ref()).unwrap_err();
        assert_eq!(error.kind, WalletErrorKind::Unsupported);
        assert!(node.events().is_empty());
    }

    #[test]
    fn validation_rejects_other_configurations_and_dependencies_without_io() {
        let (_, factory) = factory();
        let wallet = factory.attach(Node::key(1)).unwrap();
        let other = Arc::new(Node::default());
        let mut candidates = Vec::new();
        for config in [
            factory
                .config
                .clone()
                .with_address_type(AddressType::Taproot),
            factory.config.clone().with_fee_target_blocks(7),
            factory
                .config
                .clone()
                .with_max_fee_rate(FeeRate::from_sat_per_kvb(2_000)),
        ] {
            candidates.push(WalletFactory {
                config,
                ..equivalent(&factory)
            });
        }
        candidates.push(WalletFactory {
            outbox: other.factory().outbox,
            ..equivalent(&factory)
        });
        candidates.push(WalletFactory {
            utxos: Arc::new(UtxoReader::new(Network::Regtest, other.clone())),
            ..equivalent(&factory)
        });
        for candidate in candidates {
            let error = candidate.validate(wallet.as_ref()).unwrap_err();
            assert_eq!(error.kind, WalletErrorKind::Unsupported);
        }
    }

    #[test]
    fn attachment_derives_the_configured_address_type_and_keeps_the_signer() {
        for (address_type, prefix) in [
            (AddressType::SegwitV0, "bcrt1q"),
            (AddressType::Taproot, "bcrt1p"),
        ] {
            let (_, mut factory) = factory();
            factory.config = factory.config.with_address_type(address_type);
            let key = Node::key(1);
            let expected = key.address(Network::Regtest, address_type);
            let request = SignRequest {
                payload: SignablePayload::Digest(Digest { bytes: vec![9; 32] }),
                scheme: SignatureScheme::EcdsaSecp256k1,
                encoding: SignatureEncoding::Der,
                public_key_format: PublicKeyFormat::Compressed,
                key_tweak: None,
            };
            let signed = block_on(key.sign(request.clone())).unwrap();
            let wallet = factory.attach(key).unwrap();
            assert_eq!(
                wallet.address(),
                &expected.canonical(&factory.config.network)
            );
            assert!(wallet.address().value.starts_with(prefix));
            assert_eq!(
                &factory.parse_address(&wallet.address().value).unwrap(),
                wallet.address()
            );
            assert_eq!(block_on(wallet.sign(request)).unwrap(), signed);
        }
    }

    #[test]
    fn parsed_address_checks_the_network_and_canonicalizes_case() {
        let (_, factory) = factory();
        let taproot = Node::key(2).address(Network::Regtest, AddressType::Taproot);
        let canonical = taproot.canonical(&factory.config.network);
        for text in [taproot.as_str().to_owned(), taproot.as_str().to_uppercase()] {
            assert_eq!(factory.parse_address(&text).unwrap(), canonical);
        }
        let mainnet = Node::key(2).address(Network::Mainnet, AddressType::SegwitV0);
        let error = factory.parse_address(mainnet.as_str()).unwrap_err();
        assert_eq!(error.kind, WalletErrorKind::InvalidAddress);
        let expected = Address::parse(mainnet.as_str(), Network::Regtest).unwrap_err();
        assert_eq!(error.message, expected.message);
    }

    /// The store-free parse and the factory's are one parse, not two that
    /// agree: the factory's impl delegates, and the configuration a caller
    /// builds from a `Network` alone is the same value the factory holds.
    #[test]
    fn the_factorys_parse_is_the_configurations_own() {
        let (_, factory) = factory();
        let config = WalletConfig::new(&Network::Regtest);
        assert_eq!(config, factory.config);
        let taproot = Node::key(2).address(Network::Regtest, AddressType::Taproot);
        let mainnet = Node::key(2).address(Network::Mainnet, AddressType::SegwitV0);
        for text in [
            taproot.as_str().to_owned(),
            taproot.as_str().to_uppercase(),
            mainnet.as_str().to_owned(),
            String::new(),
            "not-an-address".to_owned(),
        ] {
            let reported = |error: WalletError| (error.kind, error.message);
            assert_eq!(
                config.parse_address(&text).map_err(reported),
                factory.parse_address(&text).map_err(reported),
            );
        }
    }

    /// The one parse is `crypto::SecretBytes::from_hex`; the length and the
    /// curve are this factory's `create`. Together they reject exactly what
    /// the deleted `SecretParser` rejected.
    #[test]
    fn secret_text_is_hexadecimal_in_either_case_without_disclosure() {
        let (_, factory) = factory();
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
    fn creation_and_generation_share_derivation_and_keep_secret_errors() {
        let (_, factory) = factory();
        let generated =
            block_on(Generation::of(&factory, Ok(SecretBytes::new([1_u8; 32])))).unwrap();
        let created = block_on(factory.create(SecretBytes::new([1_u8; 32]))).unwrap();
        assert_eq!(generated.wallet.address(), created.address());
        assert_eq!(generated.material.as_bytes(), [1_u8; 32]);
        assert_eq!(
            Node::key(1)
                .address(Network::Regtest, AddressType::SegwitV0)
                .canonical(&generated.wallet.address().network),
            *generated.wallet.address()
        );
        let error = block_on(factory.create(SecretBytes::new(Vec::new())))
            .err()
            .unwrap();
        assert_eq!(error.kind, WalletErrorKind::InvalidSecret);
        let error = block_on(Generation::of(
            &factory,
            Err(Error::new(ErrorKind::KeyGeneration, "no randomness").into()),
        ))
        .err()
        .unwrap();
        assert_eq!(
            (error.kind, error.message.as_str()),
            (WalletErrorKind::Generation, "no randomness")
        );
    }

    /// Membership is proved by the factory, not by a tag recorded somewhere:
    /// a wallet another factory made is refused even though it is a Bitcoin
    /// wallet of the same network.
    #[test]
    fn the_collection_accepts_attached_wallets_and_rejects_foreign_ones() {
        let (_node, factory) = factory();
        let wallet = factory.attach(Node::key(1)).unwrap();
        let foreign = Arc::new(Node::default())
            .factory()
            .attach(Node::key(2))
            .unwrap();
        let mut collection = wallets::Wallets::<&str>::new();
        collection.add("btc", factory).unwrap();

        collection.accepts(&"btc", wallet.as_ref()).unwrap();
        let error = collection.accepts(&"btc", foreign.as_ref()).unwrap_err();

        assert_eq!(error.kind, WalletErrorKind::Unsupported);
    }

    #[test]
    fn the_factory_asset_is_native_btc() {
        assert_eq!(
            factory().1.asset(),
            AssetId::native(ChainName::new("bitcoin"))
        );
    }

    fn prepared(wallet: &Wallet) -> base::SignedTransaction {
        let mut builder = wallet.transaction();
        builder
            .transfer(wallet.address().clone(), "0.00001".parse().unwrap())
            .unwrap();
        block_on(builder.prepare()).unwrap()
    }

    fn funded(node: Node) -> (Arc<Node>, Wallet) {
        let key = Node::key(1);
        let address = key.address(Network::Regtest, AddressType::SegwitV0);
        let node = Arc::new(Node {
            outputs: Ok(vec![(address.clone(), 100_000)]),
            ..node
        });
        let config = WalletConfig::new(&Network::Regtest);
        let wallet = Wallet::fixture(config, key, address, &node);
        (node, wallet)
    }

    #[test]
    fn broadcast_submits_the_prepared_envelope_and_returns_its_id() {
        let (node, wallet) = funded(Node::default());
        let prepared = prepared(&wallet);
        let submitted = block_on(wallet.broadcaster().broadcast(&prepared)).unwrap();
        assert_eq!(&submitted.id, prepared.id());
        let sent = node.submitted.lock().unwrap();
        assert_eq!(sent[0].envelope.as_bytes(), prepared.envelope().as_bytes());
        assert_eq!(node.events()[2..], ["preflight", "broadcast"]);
        let error = block_on(wallet.broadcaster().broadcast(&prepared)).unwrap_err();
        assert_eq!(
            (error.kind, error.ambiguous_transaction_id),
            (TransactionErrorKind::Rejected, None),
            "a settled transaction is not broadcast twice"
        );
    }

    #[test]
    fn only_an_unknown_broadcast_outcome_carries_the_local_id() {
        for (broadcast, kind, ambiguous) in [
            (
                Error::rpc("timed out", true),
                TransactionErrorKind::Unavailable,
                true,
            ),
            (
                Error::new(ErrorKind::Rejected, "code -26"),
                TransactionErrorKind::Rejected,
                false,
            ),
        ] {
            let (_, wallet) = funded(Node {
                broadcast: Some(broadcast),
                ..Node::default()
            });
            let prepared = prepared(&wallet);
            let error = block_on(wallet.broadcaster().broadcast(&prepared)).unwrap_err();
            assert_eq!(error.kind, kind);
            assert_eq!(error.ambiguous_transaction_id.is_some(), ambiguous);
            if ambiguous {
                assert_eq!(error.ambiguous_transaction_id.as_ref(), Some(prepared.id()));
            }
        }
    }

    #[test]
    fn preflight_failures_and_foreign_envelopes_never_broadcast() {
        let (node, wallet) = funded(Node {
            acceptance: Ok(crate::native::transaction::Acceptance {
                allowed: false,
                reject_reason: Some("missing-inputs".to_owned()),
                virtual_size: None,
                base_fee: None,
            }),
            ..Node::default()
        });
        let mut builder = wallet.transaction();
        builder
            .transfer(wallet.address().clone(), "0.00001".parse().unwrap())
            .unwrap();
        let error = block_on(builder.prepare()).unwrap_err();
        assert_eq!(
            (error.kind, error.ambiguous_transaction_id),
            (TransactionErrorKind::Rejected, None),
            "the mempool preflight runs when the transaction is signed"
        );
        let (_, wallet) = funded(Node::default());
        let prepared = prepared(&wallet);
        let foreign = base::SignedTransaction::new(
            "ethereum.signed",
            prepared.id().clone(),
            prepared.envelope().clone(),
            prepared.fee().clone(),
        );
        let error = block_on(wallet.broadcaster().broadcast(&foreign)).unwrap_err();
        assert_eq!(error.kind, TransactionErrorKind::InvalidTransaction);
        let tampered = base::SignedTransaction::new(
            TransferBuilder::PREPARED_KIND,
            base::TransactionId::new(format!("{:064x}", 9)),
            prepared.envelope().clone(),
            prepared.fee().clone(),
        );
        let error = block_on(wallet.broadcaster().broadcast(&tampered)).unwrap_err();
        assert_eq!(
            (error.kind, error.ambiguous_transaction_id),
            (TransactionErrorKind::InvalidTransaction, None)
        );
        assert!(!node.events().contains(&"broadcast".to_owned()));
    }
}
