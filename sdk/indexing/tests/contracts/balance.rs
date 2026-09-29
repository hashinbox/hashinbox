//! The balance fold contract.

use base::Decimal;

use indexing::{
    Address, AssetId, Balance, BalanceQuery, BalanceStore, BlockCredit, CanonicalBlock,
    Completeness, FeeSettlement, IndexStore, InterpretedBlock, MovementId, Network, NetworkFee,
    ObservationDraft, ObservationDraftStatus, OutputChanges, TransactionRef, ValueMovement,
};

use super::fixture::Fixture;

/// The [`BalanceStore`] contract, checked against one repository.
///
/// The fixture writes one transaction per block, heights 1 to 8, over the
/// native asset `N` and a token `T`. Every fee is `N`, and is written to the
/// store whichever rule a chain settles it under; the rule decides only
/// whether the fold applies it.
///
/// | Height | Id | Movements | Fee payer |
/// |---|---|---|---|
/// | 1 | `credit` | `N` treasury→a, 100 | treasury |
/// | 2 | `spend` | `N` a→b, 30 | a |
/// | 3 | `mint` | `T` c→a, 50 | c |
/// | 4 | `burn` | `T` a→c, 20 | a |
/// | 5 | `utxo` | `N` input a 40, output b 25, output a 10 | a |
/// | 6 | `failed` | none (failed) | a |
/// | 7 | `circle` | `N` a→a, 9 | a |
/// | 8 | `overdraw` | `N` d→e, 5 | d |
///
/// Every fee is 7 except `utxo`'s, which is 5.
///
/// One scenario adds a ninth block on top, carrying only transactionless
/// credits; the other eight run against heights 1 to 8 alone.
pub struct BalanceContract<'a, R> {
    pub(super) fixture: Fixture<'a, R>,
    settled: Fixture<'a, R>,
}

impl<'a, R: BalanceStore + IndexStore> BalanceContract<'a, R> {
    /// Checks `repository`, which must hold nothing for `network` yet.
    ///
    /// **Two chains, because the fee rule is a chain's and is stated at
    /// commit.** The same ledger is written twice: once under a chain that
    /// charges its fee beside the movements and once under a chain whose fee is
    /// already inside them. A store of effects cannot answer both from one set
    /// of rows, and that is the point — the rule is not something a reader can
    /// be told, correctly or otherwise. The second chain is a sibling network of
    /// the same store, exactly as `PurgeContract`'s is.
    #[must_use]
    pub fn new(repository: &'a R, network: Network) -> Self {
        let settled = Network::new(network.chain.clone(), format!("{}-utxo", network.name));
        Self {
            fixture: Fixture::new(repository, network),
            settled: Fixture::under(repository, settled, FeeSettlement::Movements),
        }
    }

    /// The fold is every credit less every debit of the holding's own asset,
    /// with a self-transfer netting to nothing and the chain's fee applied.
    pub async fn folds_the_movements_of_one_holding(&self) {
        self.prepare().await;
        // 100 − 30 − 40 + 10 = 40 moved, less 7 + 7 + 5 + 7 + 7 = 33 in fees.
        let native = Self::folded(&self.fixture, "a", &self.fixture.native()).await;
        assert_eq!(native.amount, Decimal::from(7_u64));
        assert_eq!(native.completeness, Completeness::Incomplete);
        assert_eq!(native.observed_at, Some(self.fixture.block(8)));
        assert_eq!(native.pending, indexing::PendingValue::none());
        // b is never a fee payer: 30 received plus a 25 output.
        let counterparty = Self::folded(&self.fixture, "b", &self.fixture.native()).await;
        assert_eq!(counterparty.amount, Decimal::from(55_u64));
    }

    /// A UTXO chain's fee is already `Σ inputs − Σ outputs`, so the fold must
    /// not apply [`NetworkFee`] again: the two rules differ by exactly the
    /// fees the holding paid, never by twice one of them.
    pub async fn never_charges_a_settled_fee_twice(&self) {
        self.prepare().await;
        let implicit = Self::folded(&self.settled, "a", &self.settled.native()).await;
        assert_eq!(implicit.amount, Decimal::from(40_u64));
        let charged = Self::folded(&self.fixture, "a", &self.fixture.native()).await;
        // 7 (spend) + 7 (burn) + 5 (utxo) + 7 (failed) + 7 (circle) = 33.
        assert_eq!(charged.amount, Decimal::from(7_u64));
    }

    /// Gas is charged in the native asset, so it debits the native holding of
    /// the payer and never the token holding at the same address.
    pub async fn charges_a_fee_to_the_fee_asset_only(&self) {
        self.prepare().await;
        for fixture in [&self.fixture, &self.settled] {
            let token = Self::folded(fixture, "a", &fixture.token()).await;
            assert_eq!(
                token.amount,
                Decimal::from(30_u64),
                "a token holding never pays the native fee"
            );
            assert_eq!(token.completeness, Completeness::Incomplete);
        }
    }

    /// A fold that would go negative is a proof the index is missing credits,
    /// not a failure: it reports [`Completeness::Overdrawn`] and no amount.
    pub async fn reports_an_overdrawn_fold_as_incomplete(&self) {
        self.prepare().await;
        let spender = Self::folded(&self.fixture, "d", &self.fixture.native()).await;
        assert_eq!(spender.completeness, Completeness::Overdrawn);
        assert_eq!(spender.amount, Decimal::zero());
        assert_eq!(spender.observed_at, Some(self.fixture.block(8)));
        let received = Self::folded(&self.fixture, "e", &self.fixture.native()).await;
        assert_eq!(received.amount, Decimal::from(5_u64));
        assert_eq!(received.completeness, Completeness::Incomplete);
    }

    /// A block credits an address with no transaction to name it, and the
    /// store folds it: a second collection beside the history rows, per
    /// asset, and able to settle a fold that was provably short
    /// (`docs/design/021` section 4.1).
    ///
    /// The ninth block credits `a` 11 native and 6 token, `v` 4 native, and
    /// `d` the 12 its fold was missing. `v` is the shape the whole record
    /// exists for: a holding with a balance and an empty transaction history.
    pub async fn folds_a_credit_no_transaction_names(&self) {
        self.prepare().await;
        self.fixture
            .repository
            .commit(
                self.fixture
                    .batch(Some(self.fixture.block(8)), Self::credited(&self.fixture)),
            )
            .await
            .expect("the credit block");

        // 7 from the eight transaction blocks, plus 11 the ninth credited.
        let native = Self::folded(&self.fixture, "a", &self.fixture.native()).await;
        assert_eq!(native.amount, Decimal::from(18_u64));
        assert_eq!(native.observed_at, Some(self.fixture.block(9)));
        // A credit names its own asset, exactly as a movement does: the
        // native credit stays out of the token holding and the token credit
        // stays out of the native one.
        let token = Self::folded(&self.fixture, "a", &self.fixture.token()).await;
        assert_eq!(token.amount, Decimal::from(36_u64));
        // No transaction ever named this address, and it still has a balance.
        let validator = Self::folded(&self.fixture, "v", &self.fixture.native()).await;
        assert_eq!(validator.amount, Decimal::from(4_u64));
        assert_eq!(validator.completeness, Completeness::Incomplete);
        // The credit lands in the same total a movement's credit does, so the
        // proof of incompleteness is discharged rather than reported forever.
        let settled = Self::folded(&self.fixture, "d", &self.fixture.native()).await;
        assert_eq!(settled.amount, Decimal::zero());
        assert_eq!(settled.completeness, Completeness::Incomplete);
    }

    /// An address the index holds nothing for is zero at the store's own
    /// checkpoint, not an error and not an absent answer.
    pub async fn an_unindexed_holding_is_zero(&self) {
        let empty = Self::folded(&self.fixture, "z", &self.fixture.native()).await;
        assert_eq!(empty.amount, Decimal::zero());
        assert_eq!(empty.observed_at, None);
        assert_eq!(empty.completeness, Completeness::Incomplete);
        self.prepare().await;
        let still_empty = Self::folded(&self.fixture, "z", &self.fixture.native()).await;
        assert_eq!(still_empty.amount, Decimal::zero());
        assert_eq!(still_empty.observed_at, Some(self.fixture.block(8)));
    }

    /// An asset of another chain, or an address of another network, is
    /// refused before anything is read.
    pub async fn refuses_a_holding_it_cannot_own(&self) {
        let foreign_asset = BalanceQuery {
            address: self.fixture.address("a"),
            asset: AssetId::native(base::ChainName::new("elsewhere")),
        };
        let error = self
            .fixture
            .repository
            .balance(foreign_asset)
            .await
            .expect_err("asset of another chain");
        assert_eq!(error.kind, indexing::IndexErrorKind::InvalidRequest);
        let other = Network::new(
            self.fixture.network.chain.clone(),
            format!("{}-other", self.fixture.network.name),
        );
        // One store holds every network, so an address of another network is
        // not a mistake to refuse — it is a holding this network has no rows
        // for, and the isolation is the row key rather than a bound handle.
        let foreign = self
            .fixture
            .repository
            .balance(BalanceQuery {
                address: Address::new(other, "a"),
                asset: self.fixture.native(),
            })
            .await
            .expect("an address of another network reads as empty");
        assert_eq!(foreign.amount, base::Decimal::from(0_u64));
        assert_eq!(
            foreign.observed_at, None,
            "it answers at the other network's own checkpoint, which this \
             fixture never moved",
        );
    }

    /// One holding of `fixture`'s own chain, folded.
    async fn folded(fixture: &Fixture<'_, R>, address: &str, asset: &AssetId) -> Balance {
        fixture
            .repository
            .balance(BalanceQuery {
                address: fixture.address(address),
                asset: asset.clone(),
            })
            .await
            .expect("folded balance")
    }

    /// Commits the fixture blocks in order on both chains, each authorised by
    /// the one before.
    ///
    /// The same ledger twice, so the only difference between the two answers is
    /// the rule their batches stated.
    pub(super) async fn prepare(&self) {
        for fixture in [&self.fixture, &self.settled] {
            let drafts = Self::drafts(fixture);
            fixture.commit(fixture.chain(drafts)).await;
        }
    }

    /// The ninth block: no transaction at all, four credits the chain made
    /// on its own, and the checkpoint that carries them.
    pub(super) fn credited(fixture: &Fixture<'_, R>) -> CanonicalBlock {
        let credit = |id: &str, asset: &AssetId, to: &str, amount: u64| BlockCredit {
            id: MovementId(id.into()),
            asset: asset.clone(),
            amount: Decimal::from(amount),
            to: fixture.address(to),
        };
        let (native, token) = (fixture.native(), fixture.token());
        let interpreted = InterpretedBlock {
            block: fixture.block(9),
            transactions: Vec::new(),
            outputs: OutputChanges::default(),
            credits: vec![
                credit("credit:1", &native, "a", 11),
                credit("credit:2", &token, "a", 6),
                credit("credit:3", &native, "v", 4),
                credit("credit:4", &native, "d", 12),
            ],
        };
        CanonicalBlock::new(fixture.network.clone(), interpreted).expect("valid credit facts")
    }

    fn drafts(fixture: &Fixture<'_, R>) -> Vec<ObservationDraft> {
        let (native, token) = (fixture.native(), fixture.token());
        vec![
            Self::draft(
                fixture,
                "credit",
                vec![Self::transfer(
                    fixture, "credit-n", &native, "treasury", "a", 100,
                )],
                "treasury",
                7,
            ),
            Self::draft(
                fixture,
                "spend",
                vec![Self::transfer(fixture, "spend-n", &native, "a", "b", 30)],
                "a",
                7,
            ),
            Self::draft(
                fixture,
                "mint",
                vec![Self::transfer(fixture, "mint-t", &token, "c", "a", 50)],
                "c",
                7,
            ),
            Self::draft(
                fixture,
                "burn",
                vec![Self::transfer(fixture, "burn-t", &token, "a", "c", 20)],
                "a",
                7,
            ),
            Self::draft(fixture, "utxo", Self::utxo(fixture, &native), "a", 5),
            ObservationDraft {
                status: ObservationDraftStatus::Failed {
                    reason: Some("reverted".into()),
                },
                ..Self::draft(fixture, "failed", Vec::new(), "a", 7)
            },
            Self::draft(
                fixture,
                "circle",
                vec![Self::transfer(fixture, "circle-n", &native, "a", "a", 9)],
                "a",
                7,
            ),
            Self::draft(
                fixture,
                "overdraw",
                vec![Self::transfer(fixture, "overdraw-n", &native, "d", "e", 5)],
                "d",
                7,
            ),
        ]
    }

    /// One spend of a 40 output into a 25 payment and a 10 change output: the
    /// missing 5 is the fee, already inside the movements.
    fn utxo(fixture: &Fixture<'_, R>, native: &AssetId) -> Vec<ValueMovement> {
        vec![
            ValueMovement::input(
                MovementId("utxo-in".into()),
                native.clone(),
                Decimal::from(40_u64),
                Some(fixture.address("a")),
            ),
            ValueMovement::output(
                MovementId("utxo-out".into()),
                native.clone(),
                Decimal::from(25_u64),
                Some(fixture.address("b")),
            ),
            ValueMovement::output(
                MovementId("utxo-change".into()),
                native.clone(),
                Decimal::from(10_u64),
                Some(fixture.address("a")),
            ),
        ]
    }

    fn draft(
        fixture: &Fixture<'_, R>,
        id: &str,
        movements: Vec<ValueMovement>,
        payer: &str,
        fee: u64,
    ) -> ObservationDraft {
        ObservationDraft {
            transaction_id: TransactionRef::new(fixture.network.clone(), id),
            transaction_order: 0,
            status: ObservationDraftStatus::Included,
            movements,
            fee: Some(NetworkFee {
                asset: fixture.native(),
                amount: Decimal::from(fee),
                payer: Some(fixture.address(payer)),
            }),
        }
    }

    fn transfer(
        fixture: &Fixture<'_, R>,
        id: &str,
        asset: &AssetId,
        from: &str,
        to: &str,
        amount: u64,
    ) -> ValueMovement {
        ValueMovement::transfer(
            MovementId(id.into()),
            asset.clone(),
            Decimal::from(amount),
            fixture.address(from),
            fixture.address(to),
        )
    }
}
