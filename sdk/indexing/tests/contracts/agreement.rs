//! Where the two shipped stores used to answer the same call differently.
//!
//! Split from `watch.rs` by role: one file per guarantee under the
//! repository's 500-line cap. Every scenario here is one the stores once
//! disagreed on — the map-backed one withdrew by the `(asset, address)` pair
//! while the file deleted by identity, and the same conflicting definition was retryable in one place and terminal
//! in another.
//!
//! **One rule decides all of them: a row has two names, its identity and its
//! `(address, asset)` pair, and no call may move one name onto a row the other
//! name does not also select.** Withdrawal is by identity; a definition
//! naming a row by only one of the two is refused, and refused for
//! good — the stored row is the caller's own earlier registration, not a race,
//! so retrying can only be refused again.

use indexing::{
    Address, IndexError, IndexErrorKind, IndexStore, Network, Watch, WatchProgress, WatchStore,
};

use super::watch::WatchContract;

impl<R: WatchStore + IndexStore> WatchContract<'_, R> {
    /// A withdrawal removes the row its identity names, and only that one.
    ///
    /// The pair `watch` carries is not a second key: forgetting another
    /// identity at a held pair removes nothing, forgetting a held identity under
    /// a stale pair still removes it, and an identity another network holds is
    /// not this network's to forget.
    pub async fn forgets_by_identity_and_never_by_pair(&self) {
        let mine = self.watch("mine", "native", "a", 1);
        let foreign = self.foreign("foreign");
        self.add(vec![mine.clone()]).await;
        let recorded = self.record(std::slice::from_ref(&mine)).await;
        self.repository
            .add(vec![foreign.clone()])
            .await
            .expect("the other network's observation");

        self.forget(&Watch {
            id: self.watch("stranger", "native", "a", 1).id,
            ..mine.clone()
        })
        .await;
        self.forget(&Watch {
            id: foreign.id.clone(),
            ..mine.clone()
        })
        .await;
        assert_eq!(
            self.stored().await,
            [WatchProgress {
                watch: mine.clone(),
                indexed_through: Some(recorded),
            }],
            "neither a stranger's identity at this pair nor another network's \
             identity removed the row, nor its progress",
        );

        self.forget(&Watch {
            address: Address::new(self.network.clone(), "stale"),
            ..mine
        })
        .await;
        assert!(
            self.stored().await.is_empty(),
            "the identity named the row, whatever pair the caller remembered",
        );
        assert_eq!(
            self.elsewhere_watches().await,
            [foreign],
            "the other network's row is untouched",
        );
    }

    /// Every conflicting definition `add` refuses is refused for good: an
    /// identity under another pair or birthday, a pair under another identity,
    /// and an identity another network holds.
    pub async fn refuses_every_conflicting_definition_for_good(&self) {
        let first = self.watch("wallet", "native", "a", 7);
        let foreign = self.foreign("shared");
        self.add(vec![first.clone()]).await;
        self.repository
            .add(vec![foreign.clone()])
            .await
            .expect("the other network's observation");

        for conflicting in [
            self.watch("wallet", "native", "b", 7),
            self.watch("wallet", "0xtoken", "a", 7),
            self.watch("wallet", "native", "a", 9),
            self.watch("second", "native", "a", 7),
            Watch {
                id: foreign.id.clone(),
                ..self.watch("ignored", "native", "c", 7)
            },
        ] {
            let error = self
                .repository
                .add(vec![conflicting])
                .await
                .expect_err("a definition under a name that is taken");
            Self::refused_for_good(&error);
        }

        assert_eq!(self.watches().await, [first], "nothing was retargeted");
        assert_eq!(self.elsewhere_watches().await, [foreign]);
    }

    /// The refusal every scenario here expects: a conflict no retry can settle.
    fn refused_for_good(error: &IndexError) {
        assert_eq!(error.kind, IndexErrorKind::Conflict);
        assert!(
            !error.retryable,
            "a stored definition is not a race, so a retry is refused again: {error:?}",
        );
    }

    /// A sibling network of the same chain, held by the same store.
    fn elsewhere(&self) -> Network {
        Network::new(
            self.network.chain.clone(),
            format!("{}-other", self.network.name),
        )
    }

    /// An observation of [`Self::elsewhere`] under the identity `id`, which no
    /// fixture of this network names.
    fn foreign(&self, id: &str) -> Watch {
        let mut watch = self.watch(id, "native", "a", 1);
        watch.address = Address::new(self.elsewhere(), "a");
        watch
    }

    /// The observations [`Self::elsewhere`] holds.
    async fn elsewhere_watches(&self) -> Vec<Watch> {
        self.repository
            .all(&self.elsewhere())
            .await
            .expect("the other network's observations")
            .into_iter()
            .map(|progress| progress.watch)
            .collect()
    }
}
