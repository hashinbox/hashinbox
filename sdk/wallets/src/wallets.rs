use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

use base::{Address, Network};
use base::{AssetId, Decimal, KeyFormat};

use crate::{
    Admission, Author, Authorship, FutureResult, Limit, Policy, Queue, SecretBytes, SendError,
    SendFuture, Stats, Suspension, Transfer, Wallet, WalletError, WalletErrorKind, WalletFactory,
    WalletRepository, pause::Gate,
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
    /// The exact bytes of the keys its factory creates and imports.
    pub key_format: KeyFormat,
}

/// Maximum authored transfer occurrences accepted by one batch.
pub const MAX_TRANSFERS: usize = 50;

/// One registered [`WalletFactory`] with the facts it declared
/// ([`crate::FactoryProfile`]), read once at registration.
#[derive(Clone)]
struct Factory {
    network: Network,
    asset: AssetId,
    key_format: KeyFormat,
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

/// Every key-holding wallet this process signs with, by wallet id and the
/// asset whose factory built it; shared by every clone of the collection.
type Signers<A> = Arc<RwLock<BTreeMap<(String, A), Arc<dyn Wallet>>>>;

/// The wallet factories of every configured asset, keyed by asset tag.
///
/// It unifies factories and holds the live signers: it makes wallets, keeps
/// the ones restored ([`Self::load_signers`]) or handed to it
/// ([`Self::insert_signer`]), and answers what each asset is. What is observed
/// is the index's, and where a key lives is the application's.
#[derive(Clone)]
pub struct Wallets<A: Ord> {
    factories: BTreeMap<A, Factory>,
    /// One pause per network an asset is registered on ([`Self::pause`]).
    gates: BTreeMap<Network, Arc<Gate>>,
    /// The live signers ([`Self::signer`]).
    signers: Signers<A>,
}

impl<A: Ord> Default for Wallets<A> {
    fn default() -> Self {
        Self {
            factories: BTreeMap::new(),
            gates: BTreeMap::new(),
            signers: Arc::default(),
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
            key_format: factory.key_format(),
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
                key_format: factory.key_format,
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
            key_format: factory.key_format,
        })
    }

    /// Restores the live signers: every active wallet of a `served` network
    /// that holds a key, rebuilt by each registered asset factory of its
    /// network that takes its key format, replacing any held before.
    /// Archived wallets sign nothing, and wallets of networks not served wait
    /// for a load that serves them. Nothing is watched or unwatched here.
    ///
    /// # Errors
    ///
    /// The repository's read, a factory's refusal of a stored key, and
    /// [`WalletErrorKind::Unavailable`] when a rebuilt key is not its
    /// wallet's address or the signers' lock is poisoned.
    pub async fn load_signers(
        &self,
        repository: &dyn WalletRepository,
        served: &[Network],
    ) -> Result<(), WalletError> {
        let stored = repository.wallets().await?;
        let assets = self.assets();
        let mut signers = BTreeMap::new();
        for wallet in stored
            .iter()
            .filter(|wallet| wallet.archived_at().is_none())
        {
            let Some(key) = wallet.key() else {
                continue;
            };
            let address = wallet.address();
            if !served.contains(&address.network) {
                continue;
            }
            let configured = assets
                .iter()
                .filter(|info| info.network == address.network && info.key_format == key.format);
            for info in configured {
                let signer = self
                    .from_pk(&info.tag, SecretBytes::new(key.secret.as_bytes()))
                    .await?;
                if signer.address() != address {
                    return Err(WalletError::new(
                        WalletErrorKind::Unavailable,
                        "a stored key is not its wallet's address",
                    ));
                }
                signers.insert((wallet.id().to_owned(), info.tag.clone()), signer);
            }
        }
        *self.signers.write().map_err(|_| Self::poisoned())? = signers;
        Ok(())
    }

    /// Keeps `signer` as wallet `id`'s for `asset`.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unavailable`] when the signers' lock is poisoned.
    pub fn insert_signer(
        &self,
        id: String,
        asset: A,
        signer: Arc<dyn Wallet>,
    ) -> Result<(), WalletError> {
        let mut signers = self.signers.write().map_err(|_| Self::poisoned())?;
        signers.insert((id, asset), signer);
        Ok(())
    }

    /// Forgets every signer of wallet `id`.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unavailable`] when the signers' lock is poisoned.
    pub fn remove_signers(&self, id: &str) -> Result<(), WalletError> {
        let mut signers = self.signers.write().map_err(|_| Self::poisoned())?;
        signers.retain(|(held, _), _| held != id);
        Ok(())
    }

    /// Wallet `id`'s signer for `asset`, if one is held.
    ///
    /// # Errors
    ///
    /// [`WalletErrorKind::Unavailable`] when the signers' lock is poisoned.
    pub fn signer(&self, id: &str, asset: &A) -> Result<Option<Arc<dyn Wallet>>, WalletError> {
        let signers = self.signers.read().map_err(|_| Self::poisoned())?;
        Ok(signers.get(&(id.to_owned(), asset.clone())).cloned())
    }

    fn poisoned() -> WalletError {
        WalletError::new(
            WalletErrorKind::Unavailable,
            "the signing keys are unavailable",
        )
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
