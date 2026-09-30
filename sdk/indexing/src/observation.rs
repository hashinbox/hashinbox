//! The vocabulary of what was observed on chain: `ValueMovement` and its
//! `MovementId`, the `NetworkFee` of a transaction, `CanonicalTransaction`
//! with its `CanonicalStatus`, and the `ObservationDraft` an interpreter
//! fills before the index accepts it. Values only; nothing here stores.

use std::collections::BTreeSet;

use base::{Decimal, Identified, MovementKind, TransactionId};

use crate::{Address, AssetId, BlockRef, IndexError, IndexErrorKind, Network, TransactionRef};

/// Chain-assigned identity of one movement, unique within its transaction
/// (for example `input:0`, `output:1`, or a log index).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MovementId(pub String);

/// One independently meaningful value change within a transaction.
///
/// Account chains report transfers, mints and burns; UTXO chains report each
/// spent input and created output separately. [`Self::amount`] is exact, in the
/// asset's smallest unit.
///
/// **The kind and its two endpoints are one fact, and are held as one.** Which
/// endpoints a movement has is decided by its kind, so [`Self::kind`],
/// [`Self::from`] and [`Self::to`] are read-only and the five constructors
/// below are the only way to pair them: a transfer cannot be built without both
/// of its addresses, a mint without its recipient, or a burn without the
/// address it shrank. That is the guarantee the five-variant enum this replaced
/// made at the type level, kept at the type level — and [`MovementKind`] was
/// already the flat discriminant every store writes, so the enum was a second
/// representation of a vocabulary that had a first.
///
/// [`Self::stored`] is the one door for a triple that was read back rather than
/// constructed, and it is checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValueMovement {
    /// Movement identity.
    pub id: MovementId,
    /// Moved asset.
    pub asset: AssetId,
    /// Moved amount.
    pub amount: Decimal,
    kind: MovementKind,
    from: Option<Address>,
    to: Option<Address>,
}

impl ValueMovement {
    /// Value moved from one address to another.
    #[must_use]
    pub fn transfer(
        id: MovementId,
        asset: AssetId,
        amount: Decimal,
        from: Address,
        to: Address,
    ) -> Self {
        Self::paired(
            id,
            asset,
            amount,
            MovementKind::Transfer,
            Some(from),
            Some(to),
        )
    }

    /// A spent UTXO, owned by an address when it has one.
    #[must_use]
    pub fn input(id: MovementId, asset: AssetId, amount: Decimal, owner: Option<Address>) -> Self {
        Self::paired(id, asset, amount, MovementKind::Input, owner, None)
    }

    /// A created UTXO, owned by an address when it has one.
    #[must_use]
    pub fn output(id: MovementId, asset: AssetId, amount: Decimal, owner: Option<Address>) -> Self {
        Self::paired(id, asset, amount, MovementKind::Output, None, owner)
    }

    /// New supply credited to an address.
    #[must_use]
    pub fn mint(id: MovementId, asset: AssetId, amount: Decimal, to: Address) -> Self {
        Self::paired(id, asset, amount, MovementKind::Mint, None, Some(to))
    }

    /// Supply destroyed from an address.
    #[must_use]
    pub fn burn(id: MovementId, asset: AssetId, amount: Decimal, from: Address) -> Self {
        Self::paired(id, asset, amount, MovementKind::Burn, Some(from), None)
    }

    /// One movement read back out of a store.
    ///
    /// The one fallible door, because a row carries the triple as three
    /// independent columns and nothing in the file's shape stops them
    /// disagreeing. A store that lost an endpoint must say so rather than hand
    /// back a transfer with one address, which would read as a credit to
    /// nobody and fold into a balance as a debit that never happened.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::Store`] when `kind` cannot carry the endpoints given —
    /// exactly the pairings the five constructors above cannot express.
    pub fn stored(
        id: MovementId,
        asset: AssetId,
        amount: Decimal,
        kind: MovementKind,
        from: Option<Address>,
        to: Option<Address>,
    ) -> Result<Self, IndexError> {
        if !Self::pairs(kind, from.as_ref(), to.as_ref()) {
            return Err(IndexError::new(
                IndexErrorKind::Store,
                "a stored movement's endpoints disagree with its kind",
                false,
            ));
        }
        Ok(Self::paired(id, asset, amount, kind, from, to))
    }

    /// Whether `kind` can carry these endpoints.
    const fn pairs(kind: MovementKind, from: Option<&Address>, to: Option<&Address>) -> bool {
        match kind {
            MovementKind::Transfer => from.is_some() && to.is_some(),
            MovementKind::Input => to.is_none(),
            MovementKind::Output => from.is_none(),
            MovementKind::Mint => from.is_none() && to.is_some(),
            MovementKind::Burn => from.is_some() && to.is_none(),
        }
    }

    fn paired(
        id: MovementId,
        asset: AssetId,
        amount: Decimal,
        kind: MovementKind,
        from: Option<Address>,
        to: Option<Address>,
    ) -> Self {
        Self {
            id,
            asset,
            amount,
            kind,
            from,
            to,
        }
    }

    /// Which kind of change this is.
    #[must_use]
    pub const fn kind(&self) -> MovementKind {
        self.kind
    }

    /// The address value left: a transfer or burn sender, or an input's owner.
    #[must_use]
    pub const fn from(&self) -> Option<&Address> {
        self.from.as_ref()
    }

    /// The address value reached: a transfer or mint recipient, or an output's
    /// owner.
    #[must_use]
    pub const fn to(&self) -> Option<&Address> {
        self.to.as_ref()
    }
}

/// The fee a transaction paid. Always in the chain's native asset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NetworkFee {
    /// The chain's native asset; anything else is rejected as `InvalidBlock`.
    pub asset: AssetId,
    /// Exact fee in the native asset's smallest unit.
    pub amount: Decimal,
    /// Who paid it, when the chain attributes the fee to one address.
    pub payer: Option<Address>,
}

impl NetworkFee {
    fn validate(&self, network: &Network) -> Result<(), IndexError> {
        let invalid_fee = self.asset.chain != network.chain
            || !self.asset.is_native()
            || self.amount.validate_amount().is_err()
            || self
                .payer
                .as_ref()
                .is_some_and(|payer| !payer.belongs_to(network));
        if invalid_fee {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "transaction contains an invalid network fee",
                false,
            ));
        }
        Ok(())
    }
}

/// Canonical state stored for a transaction. Confirmation is deliberately absent:
/// it is derived from the inclusion block and the current checkpoint when read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CanonicalStatus {
    /// Executed successfully in `block`.
    Included {
        /// The including block.
        block: BlockRef,
    },
    /// Included in `block` but failed (for example an EVM revert); it moved
    /// no value but may have paid a fee.
    Failed {
        /// The including block.
        block: BlockRef,
        /// The chain's failure reason, when it reports one.
        reason: Option<String>,
    },
}

/// A transaction as storage keeps it: validated, bound to a network and a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalTransaction {
    /// The chain's transaction identifier, which names the network it was
    /// indexed in.
    pub transaction_id: TransactionRef,
    /// The transaction's native place in its block, carried through from
    /// [`ObservationDraft::transaction_order`] unchanged.
    ///
    /// Storage keeps it and pages by it, and it is the middle component of a
    /// [`HistoryPosition`](crate::HistoryPosition). Nothing between the
    /// interpreter and the store can recompute it: a store is handed the
    /// transactions that touched a selected address and never the block's whole
    /// list.
    pub transaction_order: u64,
    /// Inclusion or failure, with the block.
    pub status: CanonicalStatus,
    /// Every stable value movement, native and token.
    pub movements: Vec<ValueMovement>,
    /// The fee, when the chain charges one.
    pub fee: Option<NetworkFee>,
}

/// Identified inside its block by the chain's own identifier: the block
/// states the network, so the identifier alone tells two of its transactions
/// apart.
impl Identified for CanonicalTransaction {
    type Id = TransactionId;

    fn id(&self) -> &TransactionId {
        &self.transaction_id.id
    }
}

impl CanonicalTransaction {
    /// The block that included the transaction.
    #[must_use]
    pub fn block(&self) -> &BlockRef {
        match &self.status {
            CanonicalStatus::Included { block } | CanonicalStatus::Failed { block, .. } => block,
        }
    }

    /// Every address the transaction touches: movement endpoints and the fee
    /// payer. Storage copies the transaction under each of them.
    #[must_use]
    pub fn addresses(&self) -> BTreeSet<Address> {
        let mut addresses = self
            .movements
            .iter()
            .flat_map(|movement| {
                movement
                    .from()
                    .cloned()
                    .into_iter()
                    .chain(movement.to().cloned())
            })
            .collect::<BTreeSet<_>>();
        if let Some(payer) = self.fee.as_ref().and_then(|fee| fee.payer.clone()) {
            addresses.insert(payer);
        }
        addresses
    }
}

/// A transaction as a [`crate::BlockInterpreter`] reports it, before
/// validation binds it to its block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationDraft {
    /// The chain's transaction identifier; unique within the block, and the
    /// one place the network being indexed is named. It must match the
    /// synchronizing network.
    pub transaction_id: TransactionRef,
    /// The transaction's native place in its block: how many transactions the
    /// chain executed before it, counting **every** one of them.
    ///
    /// **The whole block's list, never the reported subset.** An interpreter is
    /// handed the block entire and reports only the transactions that touched a
    /// selected address, so this is the one fact about block order that cannot
    /// be recovered further down: a store sees the selection and never the list.
    /// Taking the position in the complete list gives one transaction the same
    /// ordinal under every selection, which is what keeps a history page ordered
    /// by a function of the block alone and a held cursor stable. A rank over the reported subset —
    /// `enumerate()` over the filtered vector — would instead depend on which
    /// watch reached the block first, and a later, wider watch would renumber
    /// rows underneath a cursor a caller is already holding.
    ///
    /// Ethereum reads it from `transactionIndex`, which it already checks
    /// against block order and against the receipt; Bitcoin and Solana take the
    /// position in the complete `tx` / `transactions` array, whose length each
    /// chain verifies before interpreting it.
    pub transaction_order: u64,
    /// Whether it succeeded.
    pub status: ObservationDraftStatus,
    /// Every stable value movement; must be empty for a failed transaction.
    pub movements: Vec<ValueMovement>,
    /// The fee, when the chain charges one.
    pub fee: Option<NetworkFee>,
}

/// Identified inside its block exactly as the [`CanonicalTransaction`] it
/// becomes, so a repeated identifier is refused when an interpreter's output
/// is assembled and cannot reach validation.
impl Identified for ObservationDraft {
    type Id = TransactionId;

    fn id(&self) -> &TransactionId {
        &self.transaction_id.id
    }
}

impl ObservationDraft {
    pub(crate) fn canonical(
        self,
        network: &Network,
        block: &BlockRef,
    ) -> Result<CanonicalTransaction, IndexError> {
        if !self.transaction_id.belongs_to(network) {
            return Err(IndexError::new(
                IndexErrorKind::NetworkMismatch,
                "transaction belongs to another network",
                false,
            ));
        }
        // An empty identifier is refused for the same reason an empty movement
        // identity is: it is not a name, and a store keys rows by it. It is also
        // what a store relies on to reserve the empty string for a row *no*
        // transaction names, so the two namespaces cannot be made to collide by
        // an interpreter.
        if self.transaction_id.id.as_str().is_empty() {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "transaction has no identifier",
                false,
            ));
        }
        self.validate_movements(network)?;
        if let Some(fee) = &self.fee {
            fee.validate(network)?;
        }
        let status = match self.status {
            ObservationDraftStatus::Included => CanonicalStatus::Included {
                block: block.clone(),
            },
            ObservationDraftStatus::Failed { reason } => CanonicalStatus::Failed {
                block: block.clone(),
                reason,
            },
        };
        Ok(CanonicalTransaction {
            transaction_id: self.transaction_id,
            transaction_order: self.transaction_order,
            status,
            movements: self.movements,
            fee: self.fee,
        })
    }

    fn validate_movements(&self, network: &Network) -> Result<(), IndexError> {
        if matches!(self.status, ObservationDraftStatus::Failed { .. })
            && !self.movements.is_empty()
        {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "failed transaction contains movements",
                false,
            ));
        }
        let mut ids = BTreeSet::new();
        for movement in &self.movements {
            let invalid_movement = movement.id.0.is_empty()
                || !ids.insert(&movement.id)
                || movement.asset.chain != network.chain
                || movement.amount.validate_amount().is_err()
                || movement
                    .from()
                    .is_some_and(|value| !value.belongs_to(network))
                || movement
                    .to()
                    .is_some_and(|value| !value.belongs_to(network));
            if invalid_movement {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "transaction contains an invalid movement",
                    false,
                ));
            }
        }
        Ok(())
    }
}

/// Whether an interpreted transaction succeeded; the block is added later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservationDraftStatus {
    /// Executed successfully.
    Included,
    /// Included but failed.
    Failed {
        /// The chain's failure reason, when it reports one.
        reason: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use base::{BlockHash, BlockHeight, BlockPosition, ChainName};

    use super::*;
    use crate::{HistoryQuery, Holder};

    fn network() -> Network {
        Network::new(ChainName::new("test"), "mainnet")
    }

    fn address(value: &str) -> Address {
        Address::new(network(), value)
    }

    fn token() -> AssetId {
        AssetId {
            chain: network().chain,
            asset: "token".into(),
        }
    }

    /// The arity guarantee the five-variant enum made at the type level, on the
    /// one path that cannot have it: a row carries kind, sender and recipient as
    /// three independent columns.
    ///
    /// A `Transfer` with a null sender used to come back as a movement with one
    /// endpoint, which folds into a balance as a credit nobody paid. It is now
    /// `Store` corruption, which is what it is.
    #[test]
    fn a_stored_movement_whose_endpoints_disagree_with_its_kind_is_corruption() {
        let refused = [
            (MovementKind::Transfer, None, Some(address("b"))),
            (MovementKind::Transfer, Some(address("a")), None),
            (MovementKind::Mint, None, None),
            (MovementKind::Mint, Some(address("a")), Some(address("b"))),
            (MovementKind::Burn, None, None),
            (MovementKind::Burn, Some(address("a")), Some(address("b"))),
            (MovementKind::Input, Some(address("a")), Some(address("b"))),
            (MovementKind::Output, Some(address("a")), Some(address("b"))),
        ];
        for (kind, from, to) in refused {
            let error = ValueMovement::stored(
                MovementId("m".into()),
                token(),
                Decimal::from(1_u64),
                kind,
                from,
                to,
            )
            .expect_err("a triple no constructor could have produced");
            assert_eq!(error.kind, IndexErrorKind::Store);
            assert!(!error.retryable, "a rewritten row is not a retry away");
        }

        // And every pairing the five constructors do produce round-trips.
        let accepted = [
            ValueMovement::transfer(
                MovementId("t".into()),
                token(),
                Decimal::from(1_u64),
                address("a"),
                address("b"),
            ),
            ValueMovement::input(
                MovementId("i".into()),
                token(),
                Decimal::from(1_u64),
                Some(address("a")),
            ),
            ValueMovement::input(MovementId("j".into()), token(), Decimal::from(1_u64), None),
            ValueMovement::output(
                MovementId("o".into()),
                token(),
                Decimal::from(1_u64),
                Some(address("b")),
            ),
            ValueMovement::output(MovementId("p".into()), token(), Decimal::from(1_u64), None),
            ValueMovement::mint(
                MovementId("m".into()),
                token(),
                Decimal::from(1_u64),
                address("b"),
            ),
            ValueMovement::burn(
                MovementId("b".into()),
                token(),
                Decimal::from(1_u64),
                address("a"),
            ),
        ];
        for movement in accepted {
            let round_tripped = ValueMovement::stored(
                movement.id.clone(),
                movement.asset.clone(),
                movement.amount.clone(),
                movement.kind(),
                movement.from().cloned(),
                movement.to().cloned(),
            )
            .expect("what a constructor built, a row can hold");
            assert_eq!(round_tripped, movement);
        }
    }

    fn block() -> BlockRef {
        BlockRef {
            position: BlockPosition(1),
            height: BlockHeight(1),
            hash: BlockHash(vec![1]),
            parent: None,
            timestamp: None,
        }
    }

    fn draft(fee: AssetId) -> ObservationDraft {
        ObservationDraft {
            transaction_id: TransactionRef::new(network(), "tx"),
            transaction_order: 0,
            status: ObservationDraftStatus::Included,
            movements: vec![ValueMovement::transfer(
                MovementId("token".into()),
                token(),
                Decimal::from(3_u64),
                address("b"),
                address("c"),
            )],
            fee: Some(NetworkFee {
                asset: fee,
                amount: Decimal::from(1_u64),
                payer: Some(address("a")),
            }),
        }
    }

    fn query(asset: AssetId) -> HistoryQuery {
        HistoryQuery {
            address: address("a"),
            asset,
            after: None,
            limit: 10,
        }
    }

    #[test]
    fn a_network_fee_must_be_paid_in_the_native_asset() {
        let error = draft(token()).canonical(&network(), &block()).unwrap_err();
        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
        assert_eq!(error.message, "transaction contains an invalid network fee");
        let native = AssetId::native(network().chain);
        assert!(draft(native).canonical(&network(), &block()).is_ok());
    }

    #[test]
    fn a_paid_fee_selects_the_fee_asset_only_and_strips_other_movements() {
        let native = AssetId::native(network().chain);
        let paid = draft(native.clone())
            .canonical(&network(), &block())
            .unwrap();
        let selected = query(native).select(paid.clone()).expect("fee asset");
        assert!(selected.movements.is_empty());
        assert_eq!(selected.fee, paid.fee);
        assert_eq!(query(token()).select(paid), None);
    }

    #[test]
    fn a_query_asset_must_belong_to_the_address_chain() {
        let foreign = AssetId::native(ChainName::new("other"));
        let error = query(foreign).validate().unwrap_err();
        assert_eq!(error.kind, IndexErrorKind::InvalidRequest);
        assert!(query(token()).validate().is_ok());
    }
}
