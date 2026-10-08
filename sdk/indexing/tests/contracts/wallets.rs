//! The wallet-registry contract: what a store that offers
//! [`WalletRegistry`] owes an application, on the one table its watch list is.
//!
//! A wallet is filed before anything observes it, observed by the watch list,
//! and archived — never deleted — when the watch is withdrawn. These scenarios
//! hold both shipped stores to the same rules for filing, restoring and
//! refusing, and to the one promise scanning depends on: an archived or
//! never-observed row is invisible to every read and write a pass makes, while
//! the facts already indexed for its holding stay readable.

use crypto::SecretBytes;

use indexing::{
    BlockHeight, BlockPosition, Decimal, FeeSettlement, IndexError, IndexErrorKind, IndexStore,
    IndexedBatch, Network, Progress, Registration, Wallet, WalletRegistry, Watch,
    WatchProgress,
};

use super::fixture::Fixture;

/// [`WalletRegistry`] checked against one repository bound to `network`, which
/// must hold nothing yet.
pub struct WalletContract<'a, R> {
    fixture: Fixture<'a, R>,
}

impl<'a, R> WalletContract<'a, R>
where
    R: WalletRegistry + IndexStore,
{
    /// Checks `repository`, which must hold nothing for `network` yet.
    #[must_use]
    pub const fn new(repository: &'a R, network: Network) -> Self {
        Self {
            fixture: Fixture::new(repository, network),
        }
    }

    /// Reads the store's own rows with `rows` where the scenario checks them.
    #[must_use]
    pub fn reading(mut self, rows: super::Rows) -> Self {
        self.fixture.rows = Some(rows);
        self
    }

    /// An unknown pair is filed archived and unobserved, with its label and
    /// key; a repeat answers the same wallet; both reads find it.
    pub async fn files_an_unknown_pair_unobserved(&self) {
        let filed = self.register("cold", "a", Some("treasury"), Some(7)).await;
        assert_eq!(filed.id(), "cold");
        assert_eq!(filed.label(), Some("treasury"));
        assert_eq!(filed.birthday(), None, "nothing observed it yet");
        assert!(
            filed.archived_at().is_some(),
            "an unobserved row is archived"
        );
        assert_eq!(filed.watch(), None);
        assert_eq!(Self::key(&filed), Some(vec![7; 32]));

        let again = self.register("cold", "a", Some("treasury"), Some(7)).await;
        assert_eq!(
            Self::shape(&again),
            Self::shape(&filed),
            "a repeat is harmless"
        );
        assert!(
            self.observed().await.is_empty(),
            "an unobserved row is not scanned"
        );
        let read = self.wallet("cold").await.expect("filed");
        assert_eq!(Self::shape(&read), Self::shape(&filed));
        assert_eq!(Self::key(&read), Some(vec![7; 32]));
        let all = self.fixture.repository.wallets().await.expect("wallets");
        assert_eq!(all.iter().map(Wallet::id).collect::<Vec<_>>(), ["cold"]);
        assert!(self.wallet("missing").await.is_none());
    }

    /// Re-filing an archived pair restores *that* wallet: its original id and
    /// birthday, the label replaced when one is given, the key kept or set.
    pub async fn restores_an_archived_pair_under_its_original_id(&self) {
        self.register("first", "a", Some("old"), None).await;
        self.observe("first", "a", 3).await;
        self.forget("first", "a", 3).await;

        let restored = self.register("second", "a", Some("new"), Some(9)).await;
        assert_eq!(restored.id(), "first", "the original id comes back");
        assert_eq!(restored.birthday(), Some(BlockPosition(3)));
        assert!(
            restored.archived_at().is_some(),
            "still archived until watched"
        );
        assert_eq!(restored.label(), Some("new"));
        assert_eq!(Self::key(&restored), Some(vec![9; 32]), "a key is filed");

        let kept = self.register("third", "a", None, None).await;
        assert_eq!(kept.label(), Some("new"), "no label given, none replaced");
        assert_eq!(Self::key(&kept), Some(vec![9; 32]), "the key is kept");
        let error = self
            .try_register("fourth", "a", None, Some(1))
            .await
            .expect_err("another key");
        Self::refused_for_good(&error);
        assert!(self.wallet("second").await.is_none(), "no second row");
    }

    /// Every conflicting filing is refused for good, and a malformed one is
    /// the caller's mistake.
    pub async fn refuses_every_conflicting_registration_for_good(&self) {
        self.register("held", "a", None, None).await;
        self.observe("held", "a", 1).await;
        self.register("parked", "b", None, None).await;

        for (id, address) in [("other", "a"), ("held", "c"), ("parked", "c")] {
            let error = self
                .try_register(id, address, None, None)
                .await
                .expect_err("a name that is taken");
            Self::refused_for_good(&error);
        }
        let elsewhere = Network::new(
            self.fixture.network.chain.clone(),
            format!("{}-other", self.fixture.network.name),
        );
        let error = self
            .fixture
            .repository
            .register(Registration {
                address: indexing::Address::new(elsewhere, "a"),
                ..self.registration("held", "a", None, None)
            })
            .await
            .expect_err("an id another network's row holds");
        Self::refused_for_good(&error);
        let error = self
            .try_register("blank", " ", None, None)
            .await
            .expect_err("a blank address");
        assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
        let ids: Vec<_> = self.fixture.repository.wallets().await.expect("wallets");
        assert_eq!(
            ids.iter().map(Wallet::id).collect::<Vec<_>>(),
            ["held", "parked"]
        );
    }

    /// Forgetting archives: the row leaves the scan list with its claim and
    /// total, keeps its birthday, label and key, and its holding's history and
    /// balance stay readable — folded, and no longer claimed complete.
    pub async fn forgetting_archives_and_keeps_the_facts_readable(&self) {
        self.register("w", "a", Some("label"), Some(4)).await;
        self.observe("w", "a", 0).await;
        self.commit_payment("a", 1).await;
        self.record("w", 1).await;

        self.forget("w", "a", 0).await;
        assert!(self.observed().await.is_empty());
        let archived = self.wallet("w").await.expect("kept");
        assert!(archived.archived_at().is_some());
        assert_eq!(archived.birthday(), Some(BlockPosition(0)));
        assert_eq!(archived.label(), Some("label"));
        assert_eq!(Self::key(&archived), Some(vec![4; 32]));
        assert!(
            self.balance("a")
                .await
                .is_none_or(|amount| amount == Decimal::from(5_u64)),
            "folded from the ledger"
        );
        let native = self.fixture.native();
        assert!(
            self.fixture
                .history("a", &native, 10)
                .await
                .is_none_or(|history| history == ["block-1"])
        );

        self.forget("w", "a", 0).await;
        let again = self.wallet("w").await.expect("kept");
        assert_eq!(again.archived_at(), archived.archived_at(), "archived once");
    }

    /// An archived row claims nothing: a progress claim naming it is refused,
    /// and payments committed while it is archived are still in its balance
    /// once it is observed again.
    pub async fn an_archived_row_takes_no_claim_and_no_total(&self) {
        self.register("w", "a", None, None).await;
        self.commit_payment("a", 1).await;
        let error = self
            .try_record("w", 1)
            .await
            .expect_err("an unobserved row takes no claim");
        assert_eq!(error.kind, IndexErrorKind::Conflict);
        self.observe("w", "a", 0).await;
        self.forget("w", "a", 0).await;
        let error = self
            .try_record("w", 1)
            .await
            .expect_err("an archived row takes no claim");
        assert_eq!(error.kind, IndexErrorKind::Conflict);

        self.commit_payment("a", 2).await;
        assert!(
            self.balance("a")
                .await
                .is_none_or(|amount| amount == Decimal::from(10_u64))
        );
        self.observe("w", "a", 0).await;
        assert!(
            self.balance("a")
                .await
                .is_none_or(|amount| amount == Decimal::from(10_u64)),
            "the total is seeded from every fact, archived time included",
        );
        self.commit_payment("a", 3).await;
        assert!(
            self.balance("a")
                .await
                .is_none_or(|amount| amount == Decimal::from(15_u64))
        );
    }

    /// An archived row is observed again by `add` under its own id; another id
    /// cannot take its pair.
    pub async fn reactivates_through_add(&self) {
        self.register("w", "a", Some("kept"), None).await;
        let error = self
            .fixture
            .repository
            .add(vec![self.watch("intruder", "a", 2)])
            .await
            .expect_err("an archived pair is reserved for its id");
        Self::refused_for_good(&error);

        self.observe("w", "a", 2).await;
        assert_eq!(
            self.observed().await,
            [WatchProgress::unread(self.watch("w", "a", 2))]
        );
        let wallet = self.wallet("w").await.expect("kept");
        assert_eq!(wallet.archived_at(), None);
        assert_eq!(wallet.label(), Some("kept"), "observing kept the label");
    }

    /// A purge resets every claim and keeps every wallet as filed: label, key,
    /// birthday and archive.
    pub async fn purging_keeps_labels_keys_and_archives(&self) {
        self.register("active", "a", Some("on"), Some(1)).await;
        self.observe("active", "a", 0).await;
        self.register("parked", "b", Some("off"), Some(2)).await;
        self.commit_payment("a", 1).await;
        self.record("active", 1).await;
        let before = self.shapes().await;

        self.fixture
            .repository
            .purge(&self.fixture.network)
            .await
            .expect("purged");

        assert_eq!(self.shapes().await, before);
        let keys: Vec<_> = self
            .fixture
            .repository
            .wallets()
            .await
            .expect("wallets")
            .iter()
            .map(Self::key)
            .collect();
        assert_eq!(keys, [Some(vec![1; 32]), Some(vec![2; 32])]);
        assert_eq!(
            self.observed().await,
            [WatchProgress::unread(self.watch("active", "a", 0))],
            "the claim went, the observation stayed",
        );
        assert!(
            self.balance("a")
                .await
                .is_none_or(|amount| amount == Decimal::from(0_u64))
        );
    }

    /// A key comes back byte for byte, and no `Debug` of a wallet prints it.
    pub async fn a_key_round_trips_and_is_never_printed(&self) {
        self.register("k", "a", None, Some(0xee)).await;
        let read = self.wallet("k").await.expect("filed");
        assert_eq!(Self::key(&read), Some(vec![0xee; 32]));
        let all = self.fixture.repository.wallets().await.expect("wallets");
        for printed in [format!("{read:?}"), format!("{all:?}")] {
            assert!(!printed.contains("238"), "{printed}");
            assert!(printed.contains("keyed: true"), "{printed}");
        }
    }

    // --- helpers -------------------------------------------------------------

    fn registration(
        &self,
        id: &str,
        address: &str,
        label: Option<&str>,
        key: Option<u8>,
    ) -> Registration {
        Registration {
            id: id.to_owned(),
            address: self.fixture.address(address),
            asset: self.fixture.native(),
            label: label.map(str::to_owned),
            secret: key.map(|byte| SecretBytes::new([byte; 32])),
        }
    }

    async fn try_register(
        &self,
        id: &str,
        address: &str,
        label: Option<&str>,
        key: Option<u8>,
    ) -> Result<Wallet, IndexError> {
        let registration = self.registration(id, address, label, key);
        self.fixture.repository.register(registration).await
    }

    async fn register(
        &self,
        id: &str,
        address: &str,
        label: Option<&str>,
        key: Option<u8>,
    ) -> Wallet {
        self.try_register(id, address, label, key)
            .await
            .expect("a filing")
    }

    async fn wallet(&self, id: &str) -> Option<Wallet> {
        self.fixture.repository.wallet(id).await.expect("a read")
    }

    fn watch(&self, id: &str, address: &str, start: u64) -> Watch {
        Watch {
            id: id.to_owned(),
            address: self.fixture.address(address),
            asset: self.fixture.native(),
            start: BlockPosition(start),
        }
    }

    /// Observes `id` from `start`, as an index's `watch` does.
    async fn observe(&self, id: &str, address: &str, start: u64) {
        self.fixture
            .repository
            .add(vec![self.watch(id, address, start)])
            .await
            .expect("observed");
    }

    async fn forget(&self, id: &str, address: &str, start: u64) {
        self.fixture
            .repository
            .forget(&self.fixture.network, &self.watch(id, address, start))
            .await
            .expect("forgetting is idempotent");
    }

    async fn observed(&self) -> Vec<WatchProgress> {
        self.fixture
            .repository
            .all(&self.fixture.network)
            .await
            .expect("the observations")
    }

    /// Commits the next block, paying `address` 5.
    async fn commit_payment(&self, address: &str, height: u64) {
        let checkpoint = (height > 1).then(|| self.fixture.block(height - 1));
        let paid = self.fixture.paid(address, BlockHeight(height));
        let canonical = self.fixture.canonical(height, paid);
        self.fixture
            .repository
            .commit(self.fixture.batch(checkpoint, canonical))
            .await
            .expect("a fixture block");
    }

    async fn try_record(&self, id: &str, through: u64) -> Result<(), IndexError> {
        let line = self
            .fixture
            .repository
            .checkpoint(&self.fixture.network)
            .await
            .expect("the checkpoint");
        let batch = IndexedBatch::new(
            self.fixture.network.clone(),
            line,
            Vec::new(),
            vec![Progress {
                watch_id: id.to_owned(),
                expected: None,
                indexed_through: self.fixture.block(through),
            }],
            FeeSettlement::Charged,
        )
        .expect("a progress-only batch");
        self.fixture.repository.commit(batch).await.map(drop)
    }

    /// Records `id` as inspected through `through`, from nothing recorded.
    async fn record(&self, id: &str, through: u64) {
        self.try_record(id, through).await.expect("a claim");
    }

    /// `address`'s settled native total, when the store's rows can be read.
    async fn balance(&self, address: &str) -> Option<Decimal> {
        self.fixture.settled(address, &self.fixture.native()).await
    }

    /// The public facts of every wallet: id, label, birthday, whether archived.
    async fn shapes(&self) -> Vec<(String, Option<String>, Option<BlockPosition>, bool)> {
        self.fixture
            .repository
            .wallets()
            .await
            .expect("wallets")
            .iter()
            .map(Self::shape)
            .collect()
    }

    fn shape(wallet: &Wallet) -> (String, Option<String>, Option<BlockPosition>, bool) {
        (
            wallet.id().to_owned(),
            wallet.label().map(str::to_owned),
            wallet.birthday(),
            wallet.archived_at().is_some(),
        )
    }

    fn key(wallet: &Wallet) -> Option<Vec<u8>> {
        wallet.secret().map(|secret| secret.as_bytes().to_vec())
    }

    fn refused_for_good(error: &IndexError) {
        assert_eq!(error.kind, IndexErrorKind::Conflict);
        assert!(!error.retryable, "a stored filing is not a race: {error:?}");
    }
}
