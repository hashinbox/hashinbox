use std::{collections::BTreeMap, sync::Arc};

use base::{AssetId, Decimal};
use indexing::{Address, Network};

use crate::{
    Author, Authorship, FutureResult, SecretBytes, SendError, SendFuture, Transfer, Wallet,
    WalletError, WalletErrorKind, WalletFactory,
};

/// What one registered asset key is: its network and the identity of the
/// asset its wallets hold, from its factory's [`crate::FactoryProfile`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetInfo<A> {
    /// The key the factory was registered under.
    pub tag: A,
    /// The one network its wallets live in.
    pub network: Network,
    /// The identity of the asset its wallets hold. What an amount of it is
    /// worth reading is the asset's own ([`base::Units`]), not this map's.
    pub id: AssetId,
}

/// Maximum authored transfer occurrences accepted by one batch.
pub const MAX_TRANSFERS: usize = 50;

/// One registered [`WalletFactory`] with the facts it declared
/// ([`crate::FactoryProfile`]), read once at registration.
#[derive(Clone)]
struct Factory {
    network: Network,
    asset: AssetId,
    maker: Arc<dyn WalletFactory>,
    /// Absent for an asset this system receives and reports and never spends;
    /// that absence is the whole refusal.
    authorship: Option<Arc<dyn Authorship>>,
}

/// One asset's authorship with the transfers it accepted.
type Prepared = (Arc<dyn Authorship>, Vec<Transfer>);

/// The wallet factories of every configured asset, keyed by asset tag.
///
/// It unifies factories and stores nothing else: it makes wallets, hands them
/// to the caller, and answers what each asset is. Who holds a wallet is the
/// caller's question, what is observed is [`indexing::Watching`]'s, and where
/// a key lives is the application's (`docs/design/030 §8 (ex-029)`).
pub struct Wallets<A: Ord> {
    factories: BTreeMap<A, Factory>,
}

impl<A: Ord> Default for Wallets<A> {
    fn default() -> Self {
        Self {
            factories: BTreeMap::new(),
        }
    }
}

impl<A> Wallets<A>
where
    A: Clone + Ord + Send + Sync + 'static,
{
    /// An empty map. It takes no index and no storage: nothing in a factory
    /// map reads either.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers the one factory of `asset` at the composition root; its
    /// network, asset identity, and whether the system spends it at all come
    /// from the factory ([`crate::FactoryProfile`]).
    ///
    /// A factory that declares no [`Authorship`] registers an asset this
    /// system receives and reports and never spends: [`Wallets::author`] and
    /// [`Wallets::send_all`] refuse it here, at lookup, and its wallets are
    /// plain [`Wallet`]s with no send and no builder on them at all. That is
    /// the only place the refusal exists — a chain writes none.
    ///
    /// There is no registry argument: the index owns the durable list of what
    /// it observes ([`indexing::Watching`]).
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Duplicate`] when `asset` already has a factory.
    pub fn add(
        &mut self,
        asset: A,
        factory: impl WalletFactory + 'static,
    ) -> Result<(), WalletError> {
        if self.factories.contains_key(&asset) {
            return Err(WalletError::new(
                WalletErrorKind::Duplicate,
                "a wallet factory is already registered for this asset",
            ));
        }
        let registered = Factory {
            network: factory.network().clone(),
            asset: factory.asset(),
            authorship: factory.authorship(),
            maker: Arc::new(factory),
        };
        self.factories.insert(asset, registered);
        Ok(())
    }

    /// A new wallet of `asset`, returned and kept by nothing.
    ///
    /// The caller keeps it. To have it indexed, watch it:
    /// `index.watch(&id, &*wallet, Birthday::Fresh)` — and, per
    /// `docs/design/030 §8 (ex-029)` section 3.3, do that *before* filing the key, so a
    /// crash leaves a watch-only address rather than a key nothing observes.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unsupported`] for an unregistered asset;
    /// [`WalletErrorKind::Generation`] from the chain's key generation.
    pub fn create<'a>(&'a self, asset: &A) -> FutureResult<'a, Arc<dyn Wallet>> {
        let factory = self.factory(asset);
        Box::pin(async move { Ok(factory?.maker.generate().await?.wallet) })
    }

    /// The wallet of `asset` that `secret` builds, returned and kept by
    /// nothing.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unsupported`] for an unregistered asset;
    /// [`WalletErrorKind::InvalidSecret`] when the bytes are not a key of this
    /// chain's curve.
    pub fn from<'a>(&'a self, asset: &A, secret: SecretBytes) -> FutureResult<'a, Arc<dyn Wallet>> {
        let factory = self.factory(asset);
        Box::pin(async move { factory?.maker.create(secret).await })
    }

    /// A new key in `asset`'s chain format, on that chain's own curve,
    /// returned and kept by nothing.
    ///
    /// This is the key-first path: make the material, store it wherever the
    /// application keeps keys, and hand it to [`Wallets::from`] when a wallet
    /// is wanted. The wallet the factory built to derive it is dropped;
    /// nothing is registered and no address is watched.
    ///
    /// There is no matching `parse_secret`: every supported chain's import
    /// format is 32 bytes as 64 hexadecimal characters, which
    /// [`crate::SecretBytes::from_hex`] already decodes without naming a
    /// chain, and the curve check belongs to
    /// [`crate::WalletFactory::create`].
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unsupported`] for an unregistered asset;
    /// [`WalletErrorKind::Generation`] from the chain's key generation.
    pub fn secret<'a>(&'a self, asset: &A) -> FutureResult<'a, SecretBytes> {
        let factory = self.factory(asset);
        Box::pin(async move { Ok(factory?.maker.generate().await?.material) })
    }

    /// Accepts `wallet` as one of `asset`'s own.
    ///
    /// Membership is proved by [`WalletFactory::validate`]
    /// ([`crate::Origin`]) — same type, same configuration, same shared RPC
    /// and outbox handles — rather than by a tag recorded when the wallet was
    /// filed somewhere.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unsupported`] for an unregistered asset or a wallet this
    /// factory could not have made.
    pub fn accepts(&self, asset: &A, wallet: &dyn Wallet) -> Result<(), WalletError> {
        self.factory(asset)?.maker.validate(wallet)
    }

    /// Parses user-supplied address text in `asset`'s network into its
    /// canonical [`Address`]: a destination, or an address to watch.
    ///
    /// It only ever produces addresses of that asset's network, which is why
    /// no send needs a separate same-network check.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unsupported`] for an unregistered asset;
    /// [`WalletErrorKind::InvalidAddress`] from the factory's parse.
    pub fn parse(&self, asset: &A, text: &str) -> Result<Address, WalletError> {
        self.factory(asset)?.maker.parse_address(text)
    }

    /// Sends an ordered batch of 1..=[`MAX_TRANSFERS`] transfers, all from
    /// wallets `asset`'s factory made, through that factory's [`Authorship`],
    /// which is its batch [`crate::Sender`].
    ///
    /// An asset whose factory declared no [`Authorship`] is refused here, at
    /// the lookup, before any item is examined: nothing about the request was
    /// wrong, so the failure names no index.
    ///
    /// Every item is checked before anything is sent; the first failure names
    /// its index. Membership is proved by [`crate::WalletFactory::validate`]
    /// ([`crate::Origin`]) — same type, same configuration, same shared RPC
    /// and outbox handles — which is the property a batch actually needs, and
    /// a stronger one than a tag recorded when a wallet was filed.
    pub fn send_all<'a>(&'a self, asset: &A, transfers: Vec<Transfer>) -> SendFuture<'a> {
        let prepared = self.prepared(asset, transfers);
        Box::pin(async move {
            let (authorship, transfers) = prepared?;
            authorship.send(transfers).await
        })
    }

    /// `wallet`'s authoring capability in `asset`: the one way to obtain an
    /// [`Author`], and so the one way to reach a send or a transaction
    /// builder at all.
    ///
    /// Both halves are checked here and nowhere else. The asset must be one
    /// this system spends — a factory that declared no [`Authorship`] is
    /// refused at this lookup, so no unspendable asset ever reaches a chain's
    /// builder — and `wallet` must be one that asset's own factory made
    /// ([`crate::Origin`]).
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unsupported`] for an unregistered asset, for an asset
    /// this system receives and never spends, and for a wallet this factory
    /// could not have made.
    pub fn author(
        &self,
        asset: &A,
        wallet: &Arc<dyn Wallet>,
    ) -> Result<Arc<dyn Author>, WalletError> {
        let factory = self.factory(asset)?;
        // The asset first: whether it is spent at all is a fact about the
        // registration, and says nothing about the wallet that was offered.
        let authorship = Self::authorship(&factory)?;
        factory.maker.validate(wallet.as_ref())?;
        authorship.author(wallet)
    }

    /// Every registered asset key with its network and asset identity, in key
    /// order.
    #[must_use]
    pub fn assets(&self) -> Vec<AssetInfo<A>> {
        self.factories
            .iter()
            .map(|(tag, factory)| AssetInfo {
                tag: tag.clone(),
                network: factory.network.clone(),
                id: factory.asset.clone(),
            })
            .collect()
    }

    /// The registered facts of `asset`.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unsupported`] for an unregistered asset.
    pub fn asset(&self, asset: &A) -> Result<AssetInfo<A>, WalletError> {
        let factory = self.factory(asset)?;
        Ok(AssetInfo {
            tag: asset.clone(),
            network: factory.network,
            id: factory.asset,
        })
    }

    fn prepared(&self, asset: &A, transfers: Vec<Transfer>) -> Result<Prepared, SendError> {
        SendError::check_count(transfers.len())?;
        let definite = |error: WalletError| SendError::definite(error.kind, error.message);
        let factory = self.factory(asset).map_err(definite)?;
        let authorship = Self::authorship(&factory).map_err(definite)?;
        for (index, transfer) in transfers.iter().enumerate() {
            Self::check(&factory, transfer)
                .map_err(|error| SendError::item(index, Vec::new(), error))?;
        }
        SendError::check_selection(&transfers)?;
        Ok((authorship, transfers))
    }

    /// The registered asset's spending capability.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unsupported`] when its factory declared none, which is a
    /// whole-request refusal and names no item: nothing about the request was
    /// wrong, the asset is one this system only receives.
    fn authorship(factory: &Factory) -> Result<Arc<dyn Authorship>, WalletError> {
        factory.authorship.clone().ok_or_else(|| {
            WalletError::new(
                WalletErrorKind::Unsupported,
                "this asset is received and reported, and is never sent",
            )
        })
    }

    /// Accepts one item: a positive amount, a destination of the factory's own
    /// network, and a wallet the factory made.
    fn check(factory: &Factory, transfer: &Transfer) -> Result<(), WalletError> {
        if matches!(&transfer.amount, crate::Amount::Exact(amount) if amount <= &Decimal::zero()) {
            return Err(WalletError::new(
                WalletErrorKind::InvalidAmount,
                "amount must be positive",
            ));
        }
        if !transfer.to.belongs_to(&factory.network) {
            return Err(WalletError::new(
                WalletErrorKind::InvalidAddress,
                "destination address belongs to another network than the wallet",
            ));
        }
        factory.maker.validate(transfer.wallet.as_ref())
    }

    fn factory(&self, asset: &A) -> Result<Factory, WalletError> {
        self.factories.get(asset).cloned().ok_or_else(|| {
            WalletError::new(
                WalletErrorKind::Unsupported,
                "no wallet factory is registered for this asset",
            )
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use base::{
        Address, Broadcaster, ChainName, SignFuture, SignRequest, Signer, TransactionBuilder,
        TransactionError, TransactionId, TransactionSnapshot,
    };
    use futures_executor::block_on;
    use indexing::{AssetId, Holder};

    use super::*;
    use crate::{
        AddressParser, FactoryProfile, Generation, SecretBytes, SendError, SendFuture, Sender,
        SingleSender, TransactionFactory, Transfer,
    };

    /// Makes [`FixtureWallet`]s in its network and parses any non-empty text.
    /// A factory only accepts wallets of its own network, which is what
    /// `Origin` proves for a real chain.
    pub(crate) struct FixtureFactory {
        network: Network,
        sender: Arc<RecordingSender>,
        generation: Option<WalletErrorKind>,
        /// Whether its asset is one the system spends at all.
        spends: bool,
    }

    impl FixtureFactory {
        /// A factory of the example chain's `network`.
        pub(crate) fn new(network: &str) -> Self {
            Self::of(fixture_network(network))
        }

        /// A factory of any network, for a fixture that needs several chains.
        pub(crate) fn of(network: Network) -> Self {
            Self {
                network,
                sender: Arc::new(RecordingSender::default()),
                generation: None,
                spends: true,
            }
        }

        /// A factory of an asset the system receives and reports and never
        /// spends, as a chain declares one by returning no authorship.
        fn receiving(network: &str) -> Self {
            Self {
                spends: false,
                ..Self::new(network)
            }
        }
    }

    impl WalletFactory for FixtureFactory {
        fn validate(&self, wallet: &dyn Wallet) -> Result<(), WalletError> {
            if wallet.address().belongs_to(&self.network) {
                return Ok(());
            }
            Err(WalletError::new(
                WalletErrorKind::Unsupported,
                "incompatible fixture wallet",
            ))
        }

        fn create<'a>(&'a self, secret: SecretBytes) -> FutureResult<'a, Arc<dyn Wallet>> {
            let value = format!("fixture-{:02x}", secret.as_bytes().first().unwrap_or(&0));
            let wallet = FixtureWallet {
                address: Address::new(self.network.clone(), value),
                asset: AssetId::native(self.network.chain.clone()),
            };
            Box::pin(async move { Ok(Arc::new(wallet) as Arc<dyn Wallet>) })
        }

        fn generate(&self) -> FutureResult<'_, Generation> {
            if let Some(kind) = self.generation {
                return Box::pin(async move {
                    Err(WalletError::new(kind, "fixture generation failed"))
                });
            }
            Generation::of(self, Ok(SecretBytes::new([7; 32])))
        }
    }

    impl AddressParser for FixtureFactory {
        fn parse_address(&self, text: &str) -> Result<Address, WalletError> {
            if text.is_empty() {
                return Err(WalletError::new(
                    WalletErrorKind::InvalidAddress,
                    "invalid fixture address",
                ));
            }
            Ok(Address::new(self.network.clone(), text.to_lowercase()))
        }
    }

    impl FactoryProfile for FixtureFactory {
        fn network(&self) -> &Network {
            &self.network
        }

        fn asset(&self) -> AssetId {
            AssetId::native(self.network.chain.clone())
        }

        fn authorship(&self) -> Option<Arc<dyn Authorship>> {
            self.spends
                .then(|| self.sender.clone() as Arc<dyn Authorship>)
        }
    }

    /// Records every batch it is handed, so a test can check what reached it.
    #[derive(Default)]
    struct RecordingSender(Mutex<Vec<Vec<(String, String)>>>);

    impl RecordingSender {
        fn batches(&self) -> Vec<Vec<(String, String)>> {
            self.0.lock().expect("recorded batches").clone()
        }
    }

    impl Sender for RecordingSender {
        fn send<'a>(&'a self, transfers: Vec<Transfer>) -> SendFuture<'a> {
            let recorded = transfers
                .iter()
                .map(|transfer| (transfer.to.value.clone(), transfer.amount.to_string()))
                .collect();
            self.0.lock().expect("recorded batches").push(recorded);
            Box::pin(async move { Ok(vec![TransactionId::new("batch")]) })
        }
    }

    impl Authorship for RecordingSender {
        fn author(&self, wallet: &Arc<dyn Wallet>) -> Result<Arc<dyn Author>, WalletError> {
            let wallet = (wallet.as_ref() as &dyn std::any::Any)
                .downcast_ref::<FixtureWallet>()
                .ok_or_else(|| {
                    WalletError::new(WalletErrorKind::Unsupported, "not a fixture wallet")
                })?;
            Ok(Arc::new(FixtureWallet {
                address: wallet.address.clone(),
                asset: wallet.asset.clone(),
            }))
        }
    }

    /// A wallet with a fixed address and no chain.
    struct FixtureWallet {
        address: Address,
        asset: AssetId,
    }

    impl Holder for FixtureWallet {
        fn address(&self) -> &Address {
            &self.address
        }

        fn asset(&self) -> &AssetId {
            &self.asset
        }
    }

    impl Signer for FixtureWallet {
        fn sign<'a>(&'a self, _request: SignRequest) -> SignFuture<'a> {
            Box::pin(async { unreachable!("fixture wallets do not sign") })
        }
    }

    impl SingleSender for FixtureWallet {
        fn send<'a>(
            &'a self,
            _destination: Address,
            _amount: crate::Amount,
        ) -> FutureResult<'a, TransactionId> {
            Box::pin(async { Ok(TransactionId::new("single")) })
        }
    }

    impl TransactionFactory for FixtureWallet {
        fn transaction(&self) -> Box<dyn TransactionBuilder> {
            unreachable!("fixture wallets build nothing")
        }

        fn resume(
            &self,
            _snapshot: &TransactionSnapshot,
        ) -> Result<Box<dyn TransactionBuilder>, TransactionError> {
            unreachable!("fixture wallets restore nothing")
        }

        fn broadcaster(&self) -> &dyn Broadcaster {
            unreachable!("fixture wallets broadcast nothing")
        }
    }

    fn fixture_network(name: &str) -> Network {
        Network::new(ChainName::new("example"), name)
    }

    /// A map with `coin` registered on `mainnet`, and that factory's sender.
    fn map() -> (Wallets<&'static str>, Arc<RecordingSender>) {
        let factory = FixtureFactory::new("mainnet");
        let sender = factory.sender.clone();
        let mut wallets = Wallets::new();
        wallets.add("coin", factory).expect("first registration");
        (wallets, sender)
    }

    #[test]
    fn one_factory_per_asset_and_a_second_is_a_duplicate() {
        let (mut wallets, _) = map();

        let error = wallets
            .add("coin", FixtureFactory::new("mainnet"))
            .expect_err("one factory per asset");

        assert_eq!(error.kind, WalletErrorKind::Duplicate);
        assert_eq!(wallets.assets().len(), 1);
    }

    /// The map makes wallets and keeps none: nothing it exposes can find one
    /// again, because the caller is holding it.
    #[test]
    fn creating_and_importing_return_the_wallet_and_keep_nothing() {
        let (wallets, _) = map();

        let created = block_on(wallets.create(&"coin")).expect("create");
        let imported = block_on(wallets.from(&"coin", SecretBytes::new([0x2a; 32]))).expect("from");

        assert_eq!(created.address().value, "fixture-07");
        assert_eq!(imported.address().value, "fixture-2a");
        assert_eq!(*created.asset(), AssetId::native(ChainName::new("example")));
        // Two creations of one asset are two wallets; nothing is deduplicated
        // or remembered here.
        let again = block_on(wallets.create(&"coin")).expect("create again");
        assert!(!Arc::ptr_eq(&created, &again));
    }

    #[test]
    fn an_unregistered_asset_is_unsupported_everywhere() {
        let (wallets, _) = map();

        for kind in [
            block_on(wallets.create(&"missing")).err().map(|e| e.kind),
            block_on(wallets.from(&"missing", SecretBytes::new([1; 32])))
                .err()
                .map(|e| e.kind),
            block_on(wallets.secret(&"missing")).err().map(|e| e.kind),
            wallets.parse(&"missing", "a").err().map(|e| e.kind),
            wallets.asset(&"missing").err().map(|e| e.kind),
            wallets
                .author(
                    &"missing",
                    &block_on(wallets.create(&"coin")).expect("a wallet"),
                )
                .err()
                .map(|e| e.kind),
        ] {
            assert_eq!(kind, Some(WalletErrorKind::Unsupported));
        }
    }

    /// Membership is the factory's answer, not a tag recorded when a wallet
    /// was filed somewhere.
    #[test]
    fn acceptance_is_proved_by_the_factory() {
        let (mut wallets, _) = map();
        wallets
            .add("other", FixtureFactory::new("testnet"))
            .expect("second network");
        let coin = block_on(wallets.create(&"coin")).expect("create");

        wallets.accepts(&"coin", coin.as_ref()).expect("its own");
        let error = wallets
            .accepts(&"other", coin.as_ref())
            .expect_err("another factory's wallet");

        assert_eq!(error.kind, WalletErrorKind::Unsupported);
    }

    #[test]
    fn registered_assets_carry_their_network_and_asset_identity() {
        let (wallets, _) = map();

        let info = wallets.asset(&"coin").expect("registered asset");

        assert_eq!(info.tag, "coin");
        assert_eq!(info.network, fixture_network("mainnet"));
        assert_eq!(info.id, AssetId::native(ChainName::new("example")));
        assert_eq!(wallets.assets(), vec![info]);
    }

    /// The one lenient address parse, and the reason a send needs no separate
    /// same-network check: it only ever produces this asset's network.
    #[test]
    fn parsing_canonicalizes_text_in_the_assets_own_network() {
        let (wallets, _) = map();

        let parsed = wallets.parse(&"coin", "FIXTURE-Address").expect("parse");

        assert_eq!(
            parsed,
            Address::new(fixture_network("mainnet"), "fixture-address")
        );
        assert_eq!(
            wallets.parse(&"coin", "").expect_err("blank text").kind,
            WalletErrorKind::InvalidAddress
        );
    }

    /// The whole of the receive-only refusal, in one place: an asset whose
    /// factory declared no [`Authorship`] has no [`Author`] and no
    /// [`Sender`], and this lookup is where both are refused. The chain that
    /// made the wallet writes no gate, and cannot: `Arc<dyn Wallet>` carries
    /// neither capability.
    #[test]
    fn an_asset_that_declares_no_authorship_has_no_author_and_no_sender() {
        let mut wallets = Wallets::new();
        wallets
            .add("coin", FixtureFactory::new("mainnet"))
            .expect("an asset the system spends");
        wallets
            .add("token", FixtureFactory::receiving("mainnet"))
            .expect("an asset the system only receives");
        let held = block_on(wallets.create(&"token")).expect("create");
        let to = wallets.parse(&"token", "destination").expect("destination");

        let Err(refused) = wallets.author(&"token", &held) else {
            panic!("a received asset must have no authoring capability")
        };
        let batch = block_on(wallets.send_all(&"token", vec![transfer(&held, to, "1")]))
            .expect_err("and no sender to reach");

        assert_eq!(refused.kind, WalletErrorKind::Unsupported);
        assert_eq!(
            refused.message,
            "this asset is received and reported, and is never sent"
        );
        assert_eq!(batch.source.kind, WalletErrorKind::Unsupported);
        assert_eq!(
            batch.failed_index, None,
            "no item failed: the asset is one the system never sends"
        );
        let spent = block_on(wallets.create(&"coin")).expect("create");
        wallets
            .author(&"coin", &spent)
            .expect("an asset the system spends authors");
    }

    #[test]
    fn a_new_secret_is_returned_and_kept_by_nothing() {
        let (wallets, _) = map();

        let secret = block_on(wallets.secret(&"coin")).expect("secret");

        assert_eq!(secret.as_bytes(), [7; 32]);
    }

    fn transfer(wallet: &Arc<dyn Wallet>, to: Address, amount: &str) -> Transfer {
        Transfer {
            wallet: wallet.clone(),
            to,
            amount: amount.parse().expect("exact amount"),
        }
    }

    #[test]
    fn a_batch_reaches_its_assets_sender_in_authored_order() {
        let (wallets, sender) = map();
        let wallet = block_on(wallets.create(&"coin")).expect("create");
        let first = wallets.parse(&"coin", "first").expect("destination");
        let second = wallets.parse(&"coin", "second").expect("destination");

        let ids = block_on(wallets.send_all(
            &"coin",
            vec![
                transfer(&wallet, first, "1.25"),
                transfer(&wallet, second, "0.5"),
            ],
        ))
        .expect("batch");

        assert_eq!(ids, [TransactionId::new("batch")]);
        assert_eq!(
            sender.batches(),
            [vec![
                ("first".to_owned(), "1.25".to_owned()),
                ("second".to_owned(), "0.5".to_owned()),
            ]]
        );
    }

    #[test]
    fn sweep_is_last_for_its_source_and_stays_unresolved_for_the_sender() {
        let (wallets, sender) = map();
        let wallet = block_on(wallets.create(&"coin")).unwrap();
        let to = wallets.parse(&"coin", "destination").unwrap();
        for tail in ["all", "1"] {
            let batch = vec![
                transfer(&wallet, to.clone(), "all"),
                transfer(&wallet, to.clone(), tail),
            ];
            let failure = block_on(wallets.send_all(&"coin", batch)).unwrap_err();
            assert_eq!(failure.failed_index, Some(1));
            assert_eq!(failure.source.kind, WalletErrorKind::InvalidBatch);
            assert!(sender.batches().is_empty());
        }
        let batch = vec![
            transfer(&wallet, to.clone(), "1"),
            transfer(&wallet, to, "all"),
        ];
        block_on(wallets.send_all(&"coin", batch)).unwrap();
        assert_eq!(sender.batches()[0][1].1, "all");
    }

    /// Every item is checked before anything is sent, and the first failure
    /// names its authored index.
    #[test]
    fn a_batch_is_checked_completely_before_its_sender_is_reached() {
        let (mut wallets, sender) = map();
        wallets
            .add("other", FixtureFactory::new("testnet"))
            .expect("second network");
        let wallet = block_on(wallets.create(&"coin")).expect("create");
        let foreign = block_on(wallets.create(&"other")).expect("create elsewhere");
        let to = wallets.parse(&"coin", "destination").expect("destination");
        let elsewhere = wallets.parse(&"other", "destination").expect("destination");
        let good = || transfer(&wallet, to.clone(), "1");

        for (transfers, index, kind) in [
            (
                vec![good(), transfer(&wallet, to.clone(), "0")],
                1,
                WalletErrorKind::InvalidAmount,
            ),
            (
                vec![good(), transfer(&wallet, elsewhere, "1")],
                1,
                WalletErrorKind::InvalidAddress,
            ),
            (
                vec![good(), transfer(&foreign, to.clone(), "1")],
                1,
                WalletErrorKind::Unsupported,
            ),
        ] {
            let error = block_on(wallets.send_all(&"coin", transfers)).expect_err("refused batch");
            assert_eq!(error.failed_index, Some(index));
            assert_eq!(error.source.kind, kind);
            assert!(error.accepted.is_empty());
        }
        assert!(sender.batches().is_empty(), "nothing reached the sender");
    }

    #[test]
    fn a_batch_holds_one_to_fifty_transfers_of_a_registered_asset() {
        let (wallets, sender) = map();
        let wallet = block_on(wallets.create(&"coin")).expect("create");
        let to = wallets.parse(&"coin", "destination").expect("destination");
        let many = |count| {
            (0..count)
                .map(|_| transfer(&wallet, to.clone(), "1"))
                .collect::<Vec<_>>()
        };

        for count in [0, MAX_TRANSFERS + 1] {
            let error = block_on(wallets.send_all(&"coin", many(count))).expect_err("bounds");
            assert_eq!(error.source.kind, WalletErrorKind::InvalidBatch);
            assert_eq!(error.failed_index, None);
        }
        let unregistered =
            block_on(wallets.send_all(&"missing", many(1))).expect_err("unregistered asset");
        assert_eq!(unregistered.source.kind, WalletErrorKind::Unsupported);
        assert_eq!(unregistered.failed_index, None);
        for count in [1, MAX_TRANSFERS] {
            block_on(wallets.send_all(&"coin", many(count))).expect("accepted bounds");
        }
        assert_eq!(sender.batches().len(), 2);
        assert_eq!(
            SendError::check_count(MAX_TRANSFERS + 1)
                .expect_err("the shared bound")
                .source
                .kind,
            WalletErrorKind::InvalidBatch
        );
    }
}
