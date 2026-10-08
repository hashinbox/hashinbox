use std::{collections::BTreeMap, sync::Arc};

use base::{Address, Network};
use base::{AssetId, Decimal};

use crate::{
    Admission, Author, Authorship, FutureResult, Limit, Policy, Queue, SecretBytes, SendError,
    SendFuture, Stats, Suspension, Transfer, Wallet, WalletError, WalletErrorKind, WalletFactory,
    pause::Gate,
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
    /// The queue `authorship` sends through, once [`Wallets::queue`] put one
    /// in front of it.
    queue: Option<Arc<Queue<Arc<dyn Authorship>>>>,
}

/// One asset's authorship with the transfers it accepted.
type Prepared = (Arc<dyn Authorship>, Vec<Transfer>);

/// The wallet factories of every configured asset, keyed by asset tag.
///
/// It unifies factories and stores nothing else: it makes wallets, hands them
/// to the caller, and answers what each asset is. Who holds a wallet is the
/// caller's question, what is observed is the index's, and where
/// a key lives is the application's.
#[derive(Clone)]
pub struct Wallets<A: Ord> {
    factories: BTreeMap<A, Factory>,
    /// One pause per network an asset is registered on ([`Self::pause`]).
    gates: BTreeMap<Network, Arc<Gate>>,
}

impl<A: Ord> Default for Wallets<A> {
    fn default() -> Self {
        Self {
            factories: BTreeMap::new(),
            gates: BTreeMap::new(),
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

    /// Adds another collection's factories without rebuilding their shared send state.
    ///
    /// # Errors
    /// Refuses overlapping tags without changing this collection.
    pub fn extend(&mut self, other: &Self) -> Result<(), WalletError> {
        if other
            .factories
            .keys()
            .any(|tag| self.factories.contains_key(tag))
        {
            return Err(WalletError::new(
                WalletErrorKind::Duplicate,
                "duplicate wallet asset tag",
            ));
        }
        self.factories.extend(other.factories.clone());
        for (network, gate) in &other.gates {
            self.gates
                .entry(network.clone())
                .or_insert_with(|| gate.clone());
        }
        Ok(())
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
    /// it observes.
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
        self.gates.entry(factory.network().clone()).or_default();
        let registered = Factory {
            network: factory.network().clone(),
            asset: factory.asset(),
            authorship: factory.authorship(),
            queue: None,
            maker: Arc::new(factory),
        };
        self.factories.insert(asset, registered);
        Ok(())
    }

    /// Puts a send queue ([`Queue`]) in front of every spendable asset, so
    /// both its batches and its wallets' own sends wait their turn.
    ///
    /// Assets of one network share one in-flight [`Limit`] of
    /// `policy.in_flight`: they reach the same node, and the bound is about
    /// that node. Every send through the queue is admitted by its network's
    /// pause ([`Self::pause`]). An asset already queued keeps its queue.
    pub fn queue(&mut self, policy: &Policy) {
        let mut limits: BTreeMap<Network, Arc<Limit>> = BTreeMap::new();
        for factory in self.factories.values_mut() {
            let Some(inner) = factory.authorship.clone() else {
                continue;
            };
            if factory.queue.is_some() {
                continue;
            }
            let limit = limits
                .entry(factory.network.clone())
                .or_insert_with(|| Arc::new(Limit::new(policy.in_flight)))
                .clone();
            let gate = self.gates.get(&factory.network).cloned();
            let queue = Arc::new(Queue::gated(inner, policy.clone(), limit, gate));
            factory.authorship = Some(queue.clone());
            factory.queue = Some(queue);
        }
    }

    /// What every asset's queue is doing now, by asset; empty until
    /// [`Self::queue`].
    #[must_use]
    pub fn queues(&self) -> Vec<(A, Stats)>
    where
        A: Clone,
    {
        self.factories
            .iter()
            .filter_map(|(tag, factory)| Some((tag.clone(), factory.queue.as_ref()?.stats())))
            .collect()
    }

    /// Refuses every later send of every queued asset; sends already admitted
    /// are still dispatched and answered.
    pub fn close(&self) {
        for queue in self
            .factories
            .values()
            .filter_map(|factory| factory.queue.as_ref())
        {
            queue.close();
        }
    }

    /// Pauses `network`: every later queued send of its assets ([`Self::queue`])
    /// and every [`Self::admit`] is refused with [`WalletErrorKind::Paused`]
    /// until the returned value is dropped. [`Suspension::drained`] waits for the
    /// sends and admissions already running.
    ///
    /// The application's half of a quiesced window on one network, such as an
    /// online rescan by the index. A network no asset is
    /// registered on has nothing to pause.
    pub fn pause(&self, network: &Network) -> Suspension {
        self.gates
            .get(network)
            .map_or_else(Suspension::nothing, Gate::pause)
    }

    /// Whether `network` is paused ([`Self::pause`]).
    #[must_use]
    pub fn paused(&self, network: &Network) -> bool {
        self.gates.get(network).is_some_and(|gate| gate.paused())
    }

    /// Admits one piece of work on `asset`'s network that is not a send — an
    /// application registering a watch of it, say — counted until the
    /// [`Admission`] is dropped, so a [`Self::pause`] waits for it. Sends are
    /// admitted by their queue and need none.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unsupported`] for an unregistered asset;
    /// [`WalletErrorKind::Paused`] while its network is paused.
    pub fn admit(&self, asset: &A) -> Result<Admission, WalletError> {
        let network = self.factory(asset)?.network;
        let gate = self.gates.get(&network).ok_or_else(|| {
            WalletError::new(
                WalletErrorKind::Unsupported,
                "wallet asset is not registered",
            )
        })?;
        gate.admit()
    }

    /// A new wallet of `asset`, returned and kept by nothing.
    ///
    /// The caller keeps it. To have it indexed, watch it:
    /// `index.watch(&id, &*wallet, Birthday::Fresh)`. The application coordinates
    /// durable observation and custody; this collection persists neither.
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
    pub fn from_pk<'a>(
        &'a self,
        asset: &A,
        secret: SecretBytes,
    ) -> FutureResult<'a, Arc<dyn Wallet>> {
        let factory = self.factory(asset);
        Box::pin(async move { factory?.maker.create(secret).await })
    }

    /// A new key in `asset`'s chain format, on that chain's own curve,
    /// returned and kept by nothing.
    ///
    /// This is the key-first path: make the material, store it wherever the
    /// application keeps keys, and hand it to [`Wallets::from_pk`] when a wallet
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

    /// Sends one transfer from `wallet`, resolving its asset from the wallet
    /// itself: the registered asset of the wallet's network and asset identity
    /// whose factory made it ([`crate::Origin`]). Otherwise exactly
    /// [`Self::send_all`] with one item; [`Author`] stays the path for
    /// batches and queues.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unsupported`] when no registered factory made
    /// `wallet`; otherwise as [`Self::send_all`].
    pub fn send<'a>(
        &'a self,
        wallet: &Arc<dyn Wallet>,
        to: Address,
        amount: crate::Amount,
    ) -> SendFuture<'a> {
        let tag = self
            .factories
            .iter()
            .find(|(_, factory)| {
                factory.asset == *wallet.asset()
                    && factory.network == wallet.address().network
                    && factory.maker.validate(wallet.as_ref()).is_ok()
            })
            .map(|(tag, _)| tag.clone());
        let transfer = Transfer {
            wallet: Arc::clone(wallet),
            to,
            amount,
        };
        match tag {
            Some(tag) => self.send_all(&tag, vec![transfer]),
            None => Box::pin(async {
                Err(SendError::definite(
                    WalletErrorKind::Unsupported,
                    "no registered wallet factory made this wallet",
                ))
            }),
        }
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
mod tests {
    use base::AssetId;
    use base::{Address, ChainName, TransactionId};
    use futures_executor::block_on;

    use super::*;
    use crate::{
        SecretBytes, SendError, Transfer,
        testing::{Factory, Outbox, Submission},
    };

    fn network(name: &str) -> Network {
        Network::new(ChainName::new("example"), name)
    }

    /// A map with `coin` registered on `mainnet`, and that factory's outbox.
    fn map() -> (Wallets<&'static str>, Outbox) {
        let factory = Factory::new(network("mainnet"));
        let outbox = factory.outbox();
        let mut wallets = Wallets::new();
        wallets.add("coin", factory).expect("first registration");
        (wallets, outbox)
    }

    /// The destinations and amounts of every batch that reached `outbox`.
    fn batches(outbox: &Outbox) -> Vec<Vec<(String, String)>> {
        outbox
            .submissions()
            .into_iter()
            .map(|submission| match submission {
                Submission::Batch(sent) => sent
                    .into_iter()
                    .map(|(to, amount)| (to.value, amount.to_string()))
                    .collect(),
                Submission::Single(..) => panic!("a batch test sends no single transfer"),
            })
            .collect()
    }

    #[test]
    fn one_factory_per_asset_and_a_second_is_a_duplicate() {
        let (mut wallets, _) = map();

        let error = wallets
            .add("coin", Factory::new(network("mainnet")))
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
        let imported =
            block_on(wallets.from_pk(&"coin", SecretBytes::new([0x2a; 32]))).expect("from");

        assert_eq!(created.address().value, "addr-07");
        assert_eq!(imported.address().value, "addr-2a");
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
            block_on(wallets.from_pk(&"missing", SecretBytes::new([1; 32])))
                .err()
                .map(|e| e.kind),
            block_on(wallets.secret(&"missing")).err().map(|e| e.kind),
            wallets.parse(&"missing", "addr-a").err().map(|e| e.kind),
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
            .add("other", Factory::new(network("testnet")))
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
        assert_eq!(info.network, network("mainnet"));
        assert_eq!(info.id, AssetId::native(ChainName::new("example")));
        assert_eq!(wallets.assets(), vec![info]);
    }

    /// The one lenient address parse, and the reason a send needs no separate
    /// same-network check: it only ever produces this asset's network.
    #[test]
    fn parsing_canonicalizes_text_in_the_assets_own_network() {
        let (wallets, _) = map();

        let parsed = wallets.parse(&"coin", "ADDR-Mixed").expect("parse");

        assert_eq!(parsed, Address::new(network("mainnet"), "addr-mixed"));
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
            .add("coin", Factory::new(network("mainnet")))
            .expect("an asset the system spends");
        wallets
            .add("token", Factory::new(network("mainnet")).receiving())
            .expect("an asset the system only receives");
        let held = block_on(wallets.create(&"token")).expect("create");
        let to = wallets
            .parse(&"token", "addr-destination")
            .expect("destination");

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
        let (wallets, outbox) = map();
        let wallet = block_on(wallets.create(&"coin")).expect("create");
        let first = wallets.parse(&"coin", "addr-first").expect("destination");
        let second = wallets.parse(&"coin", "addr-second").expect("destination");

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
            batches(&outbox),
            [vec![
                ("addr-first".to_owned(), "1.25".to_owned()),
                ("addr-second".to_owned(), "0.5".to_owned()),
            ]]
        );
    }

    #[test]
    fn send_resolves_the_asset_from_the_wallet_and_refuses_a_stranger() {
        let (wallets, outbox) = map();
        let wallet = block_on(wallets.create(&"coin")).expect("create");
        let to = wallets.parse(&"coin", "addr-first").expect("destination");

        let ids = block_on(wallets.send(&wallet, to.clone(), "2".parse().expect("amount")))
            .expect("sent");

        assert_eq!(ids, [TransactionId::new("batch")]);
        assert_eq!(
            batches(&outbox),
            [vec![("addr-first".to_owned(), "2".to_owned())]]
        );
        let (other, _) = map();
        let refused = block_on(other.send(&wallet, to, "1".parse().expect("amount")))
            .expect_err("another map's factory did not make it");
        assert_eq!(refused.source.kind, WalletErrorKind::Unsupported);
    }

    #[test]
    fn sweep_is_last_for_its_source_and_stays_unresolved_for_the_sender() {
        let (wallets, outbox) = map();
        let wallet = block_on(wallets.create(&"coin")).unwrap();
        let to = wallets.parse(&"coin", "addr-destination").unwrap();
        for tail in ["all", "1"] {
            let batch = vec![
                transfer(&wallet, to.clone(), "all"),
                transfer(&wallet, to.clone(), tail),
            ];
            let failure = block_on(wallets.send_all(&"coin", batch)).unwrap_err();
            assert_eq!(failure.failed_index, Some(1));
            assert_eq!(failure.source.kind, WalletErrorKind::InvalidBatch);
            assert!(batches(&outbox).is_empty());
        }
        let batch = vec![
            transfer(&wallet, to.clone(), "1"),
            transfer(&wallet, to, "all"),
        ];
        block_on(wallets.send_all(&"coin", batch)).unwrap();
        assert_eq!(batches(&outbox)[0][1].1, "all");
    }

    /// Every item is checked before anything is sent, and the first failure
    /// names its authored index.
    #[test]
    fn a_batch_is_checked_completely_before_its_sender_is_reached() {
        let (mut wallets, outbox) = map();
        wallets
            .add("other", Factory::new(network("testnet")))
            .expect("second network");
        let wallet = block_on(wallets.create(&"coin")).expect("create");
        let foreign = block_on(wallets.create(&"other")).expect("create elsewhere");
        let to = wallets
            .parse(&"coin", "addr-destination")
            .expect("destination");
        let elsewhere = wallets
            .parse(&"other", "addr-destination")
            .expect("destination");
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
        assert!(batches(&outbox).is_empty(), "nothing reached the sender");
    }

    #[test]
    fn a_batch_holds_one_to_fifty_transfers_of_a_registered_asset() {
        let (wallets, outbox) = map();
        let wallet = block_on(wallets.create(&"coin")).expect("create");
        let to = wallets
            .parse(&"coin", "addr-destination")
            .expect("destination");
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
        assert_eq!(batches(&outbox).len(), 2);
        assert_eq!(
            SendError::check_count(MAX_TRANSFERS + 1)
                .expect_err("the shared bound")
                .source
                .kind,
            WalletErrorKind::InvalidBatch
        );
    }

    /// After `queue`, both send paths of the asset go through its queue: a
    /// batch and a wallet's own send are counted by it, and `close` refuses
    /// the next one.
    #[test]
    fn queued_assets_send_through_their_queue_and_close_together() {
        let (mut wallets, outbox) = map();
        assert!(wallets.queues().is_empty(), "nothing is queued until asked");
        wallets.queue(&crate::Policy::default());
        let wallet =
            block_on(wallets.from_pk(&"coin", SecretBytes::new([1; 32]))).expect("a wallet");
        let to = Address::new(network("mainnet"), "addr-02");

        let batch = Transfer {
            wallet: wallet.clone(),
            to: to.clone(),
            amount: crate::Amount::All,
        };
        block_on(wallets.send_all(&"coin", vec![batch])).expect("the batch");
        let author = wallets.author(&"coin", &wallet).expect("an author");
        block_on(author.send(to.clone(), crate::Amount::All)).expect("the single send");

        assert_eq!(outbox.submissions().len(), 2);
        assert_eq!(wallets.queues()[0].1.completed, 2);

        wallets.close();
        let refused = block_on(author.send(to, crate::Amount::All)).expect_err("closed");
        assert_eq!(refused.kind, WalletErrorKind::Unavailable);
        assert_eq!(outbox.submissions().len(), 2, "nothing reached the double");
    }

    /// A paused network refuses its queued sends and its admissions, waits
    /// for the send it already admitted, leaves every other network alone,
    /// and admits again once the pause is dropped.
    #[tokio::test]
    async fn a_paused_network_drains_refuses_and_reopens() {
        let (mut wallets, outbox) = map();
        wallets
            .add("other", Factory::new(network("testnet")))
            .expect("a second network");
        wallets.queue(&crate::Policy::default());
        let wallet = wallets
            .from_pk(&"coin", SecretBytes::new([1; 32]))
            .await
            .expect("a wallet");
        let author = wallets.author(&"coin", &wallet).expect("an author");
        let to = Address::new(network("mainnet"), "addr-02");

        outbox.hold();
        let running = tokio::spawn({
            let author = author.clone();
            let to = to.clone();
            async move { author.send(to, crate::Amount::All).await }
        });
        while outbox.submissions().is_empty() {
            tokio::task::yield_now().await;
        }
        let paused = wallets.pause(&network("mainnet"));
        assert!(wallets.paused(&network("mainnet")));
        assert!(!wallets.paused(&network("testnet")));
        let refused = author
            .send(to.clone(), crate::Amount::All)
            .await
            .expect_err("paused");
        assert_eq!(refused.kind, WalletErrorKind::Paused);
        let batch = vec![transfer(&wallet, to.clone(), "1")];
        let batched = wallets.send_all(&"coin", batch).await.expect_err("paused");
        assert_eq!(batched.source.kind, WalletErrorKind::Paused);
        assert_eq!(
            wallets.admit(&"coin").err().map(|e| e.kind),
            Some(WalletErrorKind::Paused)
        );
        drop(wallets.admit(&"other").expect("another network admits"));
        assert_eq!(
            wallets.admit(&"missing").err().map(|e| e.kind),
            Some(WalletErrorKind::Unsupported)
        );

        {
            let drained = paused.drained();
            tokio::pin!(drained);
            assert!(
                poll_once(drained.as_mut()).await.is_none(),
                "the admitted send is still with the node"
            );
            outbox.release();
            running
                .await
                .expect("joined")
                .expect("the admitted send is answered");
            drained.await;
        }
        assert_eq!(
            outbox.submissions().len(),
            1,
            "nothing refused reached the node"
        );

        drop(paused);
        author
            .send(to, crate::Amount::All)
            .await
            .expect("sends again");
        wallets.pause(&network("unregistered")).drained().await;
    }

    /// One poll of `future`, without waiting.
    async fn poll_once<F: std::future::Future + Unpin>(future: F) -> Option<F::Output> {
        let mut future = future;
        std::future::poll_fn(|context| {
            std::task::Poll::Ready(match std::pin::Pin::new(&mut future).poll(context) {
                std::task::Poll::Ready(output) => Some(output),
                std::task::Poll::Pending => None,
            })
        })
        .await
    }
}
