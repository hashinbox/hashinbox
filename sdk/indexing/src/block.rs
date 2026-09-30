//! What a block says, in chain-neutral terms: `BlockCredit` (value a block
//! moves with no transaction to name it), `Facts` (one block with its output
//! changes and credits, over whichever transaction type its stage has
//! reached), `InterpretedBlock` (the facts a chain's interpreter returns) and
//! `CanonicalBlock` (the validated facts the index commits). Vocabulary only:
//! no storage, no policy, no I/O.

use std::collections::BTreeSet;

use crate::{
    Address, AssetId, Block, BlockParent, BlockRef, CanonicalTransaction, Decimal, Identified,
    IndexError, IndexErrorKind, MovementId, Network, Networked, ObservationDraft, OutputChanges,
    Transactions,
};

/// One value change a block makes with no transaction to name it.
///
/// Post-Merge Ethereum credits a consensus-layer withdrawal straight from the
/// block body: no transaction, no receipt, no trace entry. A fold over transactions alone therefore
/// understates such an address by every withdrawal it ever received, silently
/// and for good.
///
/// It is a record of its own rather than a synthetic
/// [`TransactionRef`](crate::TransactionRef), because that type means *the
/// chain's transaction identifier* and a reserved string prefix every reader
/// must learn is worse than a field. Nothing
/// here can be mistaken for a transaction, which is the point: a caller that
/// asks "which transaction did this" gets no answer because there is none.
///
/// Credit-only on purpose. No chain this SDK supports debits an address
/// without a transaction, and a signed block-level record would be a
/// generalisation with no case behind it; one earns a `from` field when one
/// appears.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockCredit {
    /// Identity within the block, derived from canonical chain data so a
    /// re-inspection of the same block produces the same one — Ethereum's is
    /// `withdrawal:<consensus index>`.
    pub id: MovementId,
    /// The credited asset; must belong to the block's chain.
    pub asset: AssetId,
    /// The credited amount, in the asset's smallest unit.
    pub amount: Decimal,
    /// The credited address.
    pub to: Address,
}

impl BlockCredit {
    /// Rejects a credit no store should hold: a blank identity, an asset of
    /// another chain, a negative amount, or an address of another network.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidBlock`] for any of them.
    pub(crate) fn validate(&self, network: &Network) -> Result<(), IndexError> {
        if self.id.0.is_empty()
            || self.asset.chain != network.chain
            || self.amount.validate_amount().is_err()
            || !self.to.belongs_to(network)
        {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "block contains an invalid credit",
                false,
            ));
        }
        Ok(())
    }

    /// Validates every credit of one block and rejects a repeated identity.
    ///
    /// Uniqueness is enforced here rather than in the fold: a store keys a
    /// credit by `(address, height, id)`, so two credits sharing an identity
    /// inside one block would collapse into one row and lose value. Two
    /// credits at *different* heights may share an identity — that is a chain
    /// numbering per block rather than per chain — and both are kept.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidBlock`] for an invalid or repeated credit.
    pub(crate) fn validate_all(credits: &[Self], network: &Network) -> Result<(), IndexError> {
        let mut ids = BTreeSet::new();
        for credit in credits {
            credit.validate(network)?;
            if !ids.insert((&credit.to, &credit.id)) {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "block contains a duplicate credit",
                    false,
                ));
            }
        }
        Ok(())
    }
}

/// Everything one block says in chain-neutral terms: the block with its
/// transactions, the live-output changes it made, and the value it moved with
/// no transaction to name it.
///
/// One shape for both stages of inspection, and the stage is the transaction
/// type. Over [`ObservationDraft`] it is what an interpreter reports
/// ([`InterpretedBlock`]); over [`CanonicalTransaction`] it is what
/// [`CanonicalBlock`] holds once validation has bound every transaction to the
/// block. The output changes and the credits are the same values at both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts<T: Identified> {
    /// The inspected block in its network, with every transaction that touched
    /// a selected address, in block order, each carrying its own native place
    /// in the block it came from ([`ObservationDraft::transaction_order`])
    /// rather than its place in this list.
    pub block: Block<T>,
    /// Live-output changes (UTXO chains); empty for account chains.
    pub outputs: OutputChanges,
    /// Value the block moved to a selected address with no transaction to
    /// name it, in block order. Empty on every chain but Ethereum.
    pub credits: Vec<BlockCredit>,
}

/// What a chain's interpreter returns for one native block: facts nothing has
/// validated yet.
pub type InterpretedBlock = Facts<ObservationDraft>;

impl<T: Identified> Facts<T> {
    /// Assembles the block `reference` names in `network` from
    /// `transactions`, kept in the given order, with no output changes and no
    /// credits; a chain that has either sets the field.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidBlock`] when two transactions share an
    /// identifier: a store keys a transaction by it, so the second would
    /// overwrite the first.
    pub fn new(
        network: &impl Networked,
        reference: BlockRef,
        transactions: Vec<T>,
    ) -> Result<Self, IndexError> {
        let transactions = Transactions::new(transactions).map_err(|_| {
            IndexError::new(
                IndexErrorKind::InvalidBlock,
                "block contains a duplicate transaction",
                false,
            )
        })?;
        Ok(Self {
            block: Block::new(network, reference, transactions),
            outputs: OutputChanges::default(),
            credits: Vec::new(),
        })
    }

    /// Checks that interpretation did not change which block it read.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidBlock`] when the reference differs.
    pub fn validate_reference(&self, source: &BlockRef) -> Result<(), IndexError> {
        if self.block.reference() != source {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "interpreter changed the block reference",
                false,
            ));
        }
        Ok(())
    }
}

/// One block's validated indexing facts: its reference, the transactions and
/// output changes it made, and the value it moved with no transaction to name
/// it.
///
/// The step after [`InterpretedBlock`], and the last one before the facts are
/// either read or written: every draft has become a
/// [`CanonicalTransaction`](crate::CanonicalTransaction), the output changes do
/// not overlap, no two transactions share a native place, and every credit is
/// valid and uniquely identified.
///
/// The same [`Facts`] an interpreter reports, sealed: the fields are private
/// and [`Self::new`] is the only constructor, so facts that were not validated
/// cannot be given this type.
///
/// It holds no raw protocol payload — those are the chain's type and megabytes
/// apiece, and the native-block cache in `fetch.rs` keeps them for the one path that
/// wants one again — and no database precondition. Which checkpoint storage
/// must hold before these facts may be written is a property of the *write*,
/// and lives on [`IndexedBatch`](crate::IndexedBatch); the facts themselves
/// are equally what a pending-block read answers from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalBlock {
    facts: Facts<CanonicalTransaction>,
}

impl CanonicalBlock {
    /// Validates `interpreted` as `network`'s facts and binds every
    /// transaction to the block: non-overlapping and uniquely identified
    /// output changes, one transaction per native place, network-bound
    /// movements and fees, and uniquely identified block credits. The
    /// reference, the output changes and the credits are kept as they came.
    ///
    /// Transaction identifiers are already unique: [`Facts::new`] refused a
    /// repeated one when the interpreter's output was assembled.
    ///
    /// Nothing here consults storage or a node, and nothing here knows where
    /// the block will be written.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidBlock`] for invalid content, and
    /// [`IndexErrorKind::NetworkMismatch`] for a block or a transaction of
    /// another network.
    pub fn new(network: Network, interpreted: InterpretedBlock) -> Result<Self, IndexError> {
        let Facts {
            block,
            outputs,
            credits,
        } = interpreted;
        let (stated, reference, drafts) = block.into_parts();
        if stated != network {
            return Err(IndexError::new(
                IndexErrorKind::NetworkMismatch,
                "block belongs to another network",
                false,
            ));
        }
        outputs.validate(&network, reference.height)?;
        let mut orders = BTreeSet::<u64>::new();
        let mut transactions = Vec::with_capacity(drafts.len());
        for draft in drafts {
            // Two transactions of one block cannot occupy one native place, so a
            // repeated ordinal says the interpreter numbered the list it
            // *reported* rather than the list the chain executed — which is the
            // one thing `ObservationDraft::transaction_order` may not be, and the
            // only moment anything still holds both lists to tell.
            if !orders.insert(draft.transaction_order) {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "block contains two transactions at one native place",
                    false,
                ));
            }
            transactions.push(draft.canonical(&network, &reference)?);
        }
        BlockCredit::validate_all(&credits, &network)?;

        Ok(Self {
            facts: Facts {
                outputs,
                credits,
                ..Facts::new(&network, reference, transactions)?
            },
        })
    }

    /// The network the block belongs to.
    #[must_use]
    pub const fn network(&self) -> &Network {
        self.facts.block.network()
    }

    /// The block these facts were read from.
    #[must_use]
    pub const fn block(&self) -> &BlockRef {
        self.facts.block.reference()
    }

    /// The block's validated transactions in block order, with inclusion
    /// status.
    #[must_use]
    pub const fn transactions(&self) -> &Transactions<CanonicalTransaction> {
        self.facts.block.transactions()
    }

    /// The block's live-output changes.
    #[must_use]
    pub const fn outputs(&self) -> &OutputChanges {
        &self.facts.outputs
    }

    /// The value the block moved with no transaction to name it, in block
    /// order.
    #[must_use]
    pub fn credits(&self) -> &[BlockCredit] {
        &self.facts.credits
    }

    /// Checks that this block extends `checkpoint`: height one above it,
    /// position above it, and parent linkage to exactly it.
    ///
    /// A block with no checkpoint to extend connects vacuously — it is the
    /// network's first.
    ///
    /// # Errors
    ///
    /// A retryable [`IndexErrorKind::Disconnected`] when it does not extend
    /// the checkpoint, and [`IndexErrorKind::InvalidBlock`] when the
    /// checkpoint's height has no successor.
    pub(crate) fn connects_to(&self, checkpoint: Option<&BlockRef>) -> Result<(), IndexError> {
        match checkpoint {
            Some(checkpoint) if !extends(self.block(), checkpoint)? => Err(
                IndexError::disconnected("block does not connect to the checkpoint"),
            ),
            _ => Ok(()),
        }
    }
}

/// Whether `block` is the produced block directly after `previous`: above it
/// in position, one above it in height, and naming exactly it as its parent.
///
/// **The linkage rule, stated once.** A reading checks each block it fetches
/// against the one before it ([`crate::scan::Scan`]) and a batch checks each
/// block it carries against the line it extends
/// ([`CanonicalBlock::connects_to`]); both ask here, so a block cannot be
/// accepted into memory under one rule and refused by a commit under another.
/// What a broken link *means* differs — a fork in flight, a batch that does not
/// fit its checkpoint — so each caller words its own refusal.
///
/// Positions may be sparse, so adjacency is by height and the parent pair, and
/// position only has to rise.
///
/// # Errors
///
/// A terminal [`IndexErrorKind::InvalidBlock`] when `previous` is at the last
/// height there is, so nothing can follow it.
pub(crate) fn extends(block: &BlockRef, previous: &BlockRef) -> Result<bool, IndexError> {
    let height = previous.height.checked_successor().ok_or_else(|| {
        IndexError::new(
            IndexErrorKind::InvalidBlock,
            "checkpoint height is exhausted",
            false,
        )
    })?;
    let parent = BlockParent {
        position: previous.position,
        hash: previous.hash.clone(),
    };
    Ok(block.position > previous.position
        && block.height == height
        && block.parent.as_ref() == Some(&parent))
}

#[cfg(test)]
mod tests {
    use base::ChainName;

    use super::*;
    use crate::{
        AssetId, BlockHash, BlockHeight, BlockPosition, CanonicalStatus, IndexedOutput, NetworkFee,
        ObservationDraftStatus, OutputId, OutputKey, TransactionRef, ValueMovement,
    };

    fn network() -> Network {
        Network::new(ChainName::new("test"), "mainnet")
    }

    fn block(height: u64) -> BlockRef {
        BlockRef {
            position: BlockPosition(height),
            height: BlockHeight(height),
            hash: BlockHash(vec![height as u8]),
            parent: height.checked_sub(1).map(|value| BlockParent {
                position: BlockPosition(value),
                hash: BlockHash(vec![value as u8]),
            }),
            timestamp: None,
        }
    }

    fn asset() -> AssetId {
        AssetId::native(network().chain)
    }

    /// Each clause of the linkage rule decides alone: a block that gets two of
    /// the three right does not extend the one before it.
    ///
    /// The scenarios that reach this rule through a reading or a batch break
    /// several clauses at once, so none of them notices one clause going
    /// missing. This does, for both callers, because there is one rule.
    #[test]
    fn a_block_extends_another_only_by_position_height_and_parent() {
        let previous = block(4);
        let next = block(5);
        assert_eq!(extends(&next, &previous), Ok(true));

        // Positions are sparse, so a gap in them is not a gap in the chain.
        let mut sparse = next.clone();
        sparse.position = BlockPosition(9);
        assert_eq!(extends(&sparse, &previous), Ok(true));

        let mut behind = next.clone();
        behind.position = previous.position;
        let mut skipped = next.clone();
        skipped.height = BlockHeight(6);
        let mut forked = next.clone();
        forked.parent = Some(BlockParent {
            position: previous.position,
            hash: BlockHash(vec![9]),
        });
        let mut misplaced = next.clone();
        misplaced.parent = Some(BlockParent {
            position: BlockPosition(3),
            hash: previous.hash.clone(),
        });
        let mut orphan = next;
        orphan.parent = None;
        for broken in [behind, skipped, forked, misplaced, orphan] {
            assert_eq!(extends(&broken, &previous), Ok(false), "{broken:?}");
        }

        let mut last = block(4);
        last.height = BlockHeight(u64::MAX);
        let error = extends(&block(5), &last).expect_err("no height follows the last");
        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
        assert!(!error.retryable);
    }

    fn address() -> Address {
        Address::new(network(), "owner")
    }

    fn output_key(id: &str) -> OutputKey {
        OutputKey {
            address: address(),
            output: OutputId {
                transaction: TransactionRef::new(network(), id),
                index: 0,
            },
        }
    }

    fn draft(amount: Decimal) -> ObservationDraft {
        ObservationDraft {
            transaction_id: TransactionRef::new(network(), "transaction"),
            transaction_order: 0,
            status: ObservationDraftStatus::Included,
            movements: vec![ValueMovement::mint(
                MovementId("movement".into()),
                asset(),
                amount,
                address(),
            )],
            fee: None,
        }
    }

    fn interpreted(
        transactions: Vec<ObservationDraft>,
        outputs: OutputChanges,
    ) -> InterpretedBlock {
        InterpretedBlock {
            outputs,
            ..InterpretedBlock::new(&network(), block(1), transactions).expect("unique ids")
        }
    }

    /// A valid draft of its own: its own identifier and its own native place.
    fn placed(id: &str, order: u64) -> ObservationDraft {
        ObservationDraft {
            transaction_id: TransactionRef::new(network(), id),
            transaction_order: order,
            ..draft(Decimal::zero())
        }
    }

    fn refusal(interpreted: InterpretedBlock) -> IndexError {
        CanonicalBlock::new(network(), interpreted).expect_err("invalid facts")
    }

    fn credit(id: &str, amount: &str) -> BlockCredit {
        BlockCredit {
            id: MovementId(id.into()),
            asset: AssetId::native(network().chain),
            amount: amount.parse().expect("a decimal"),
            to: Address::new(network(), "beneficiary"),
        }
    }

    /// Every way a credit can be one no store should hold, and the one shape
    /// that must survive: two credits of the same identity at one block are a
    /// row that would collapse, while the same identity for two *different*
    /// addresses is two rows and legal.
    #[test]
    fn a_credit_is_rejected_when_it_is_blank_foreign_negative_or_repeated() {
        for (invalid, what) in [
            (credit("", "1"), "a blank identity"),
            (credit("withdrawal:1", "-1"), "a negative amount"),
            (
                BlockCredit {
                    asset: AssetId::native(ChainName::new("elsewhere")),
                    ..credit("withdrawal:1", "1")
                },
                "an asset of another chain",
            ),
            (
                BlockCredit {
                    to: Address::new(Network::new(network().chain, "other"), "beneficiary"),
                    ..credit("withdrawal:1", "1")
                },
                "an address of another network",
            ),
        ] {
            let error = BlockCredit::validate_all(&[invalid], &network()).expect_err(what);
            assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
            assert_eq!(error.message, "block contains an invalid credit", "{what}");
        }

        let repeated = [credit("withdrawal:1", "1"), credit("withdrawal:1", "2")];
        let error =
            BlockCredit::validate_all(&repeated, &network()).expect_err("one identity, one row");
        assert_eq!(error.message, "block contains a duplicate credit");

        let shared = [
            credit("withdrawal:1", "1"),
            BlockCredit {
                to: Address::new(network(), "other-beneficiary"),
                ..credit("withdrawal:1", "2")
            },
        ];
        BlockCredit::validate_all(&shared, &network())
            .expect("one identity per address is two rows, not a collision");
    }

    /// Every way one block's output changes can be ones no store should hold:
    /// a key both required and merely tracked, two created outputs under one
    /// identity, and a negative amount.
    #[test]
    fn rejects_output_changes_that_overlap_repeat_or_go_negative() {
        let key = output_key("spent");
        let created = |owner: &str, amount: &str| IndexedOutput {
            id: output_key("created").output,
            address: Address::new(network(), owner),
            asset: asset(),
            amount: amount.parse().expect("a decimal"),
            evidence: Vec::new(),
            created_at: BlockHeight(1),
            coinbase: false,
        };
        for (outputs, what) in [
            (
                OutputChanges {
                    created: Vec::new(),
                    spent: vec![key.clone()],
                    tracked_spends: vec![key],
                },
                "a key both required and tracked",
            ),
            (
                OutputChanges {
                    created: vec![created("first", "1"), created("second", "1")],
                    spent: Vec::new(),
                    tracked_spends: Vec::new(),
                },
                "two outputs under one identity",
            ),
            (
                OutputChanges {
                    created: vec![created("owner", "-1")],
                    spent: Vec::new(),
                    tracked_spends: Vec::new(),
                },
                "a negative output",
            ),
        ] {
            let error = refusal(interpreted(Vec::new(), outputs));
            assert_eq!(error.kind, IndexErrorKind::InvalidBlock, "{what}");
        }
    }

    #[test]
    fn rejects_negative_movement_and_fee_amounts() {
        let negative = "-1".parse::<Decimal>().expect("negative decimal");
        let movement = refusal(interpreted(
            vec![draft(negative.clone())],
            OutputChanges::default(),
        ));

        let mut fee_draft = draft(Decimal::zero());
        fee_draft.fee = Some(NetworkFee {
            asset: asset(),
            amount: negative,
            payer: Some(address()),
        });
        let fee = refusal(interpreted(vec![fee_draft], OutputChanges::default()));

        assert_eq!(movement.kind, IndexErrorKind::InvalidBlock);
        assert_eq!(fee.kind, IndexErrorKind::InvalidBlock);
    }

    /// The output changes are the whole block's, so they are checked before
    /// any single transaction is: a block whose outputs overlap is invalid
    /// whichever transaction produced them.
    #[test]
    fn output_changes_are_validated_before_transactions() {
        let key = output_key("spent");
        let error = refusal(interpreted(
            vec![draft("-1".parse().expect("negative decimal"))],
            OutputChanges {
                spent: vec![key.clone(), key],
                ..OutputChanges::default()
            },
        ));

        assert_eq!(
            error,
            IndexError::new(
                IndexErrorKind::InvalidBlock,
                "block contains overlapping or duplicate output changes",
                false,
            )
        );
    }

    /// The refusal `CanonicalBlock::new` used to make, one stage earlier: two
    /// transactions under one identifier never become facts at all, so nothing
    /// downstream — not validation, not a store — has to look for them.
    ///
    /// It is decided by the identifier alone and before any draft is
    /// validated, so the order of the pair and whether either is valid on its
    /// own change nothing. Distinct identifiers are kept, in the order given.
    #[test]
    fn two_transactions_under_one_id_are_refused_where_the_facts_are_assembled() {
        let valid = draft(Decimal::zero());
        let invalid = draft("-1".parse().expect("negative decimal"));
        let apart = ObservationDraft {
            transaction_order: 9,
            ..valid.clone()
        };
        for transactions in [
            vec![valid.clone(), invalid.clone()],
            vec![invalid, valid.clone()],
            vec![placed("other", 1), valid.clone(), placed("third", 2), apart],
        ] {
            let error = InterpretedBlock::new(&network(), block(1), transactions)
                .expect_err("one identifier, two transactions");
            assert_eq!(
                error,
                IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "block contains a duplicate transaction",
                    false,
                )
            );
        }

        let kept = InterpretedBlock::new(
            &network(),
            block(1),
            vec![placed("second", 5), placed("first", 2), valid],
        )
        .expect("three identifiers");
        let ids = kept.block.transactions().iter();
        assert_eq!(
            ids.map(|draft| draft.transaction_id.id.as_str())
                .collect::<Vec<_>>(),
            ["second", "first", "transaction"]
        );
        assert_eq!(kept.block.reference(), &block(1));
        assert_eq!(kept.block.network(), &network());
    }

    /// An interpreter is handed one block and must answer about that block.
    /// Any field of the reference counts: a moved hash is another block, and
    /// a moved timestamp is the same block misreported.
    #[test]
    fn facts_about_another_block_than_the_one_read_are_refused() {
        let facts = interpreted(vec![placed("first", 0)], OutputChanges::default());
        facts
            .validate_reference(&block(1))
            .expect("the block that was read");

        let mut rehashed = block(1);
        rehashed.hash = BlockHash(vec![9]);
        let mut retimed = block(1);
        retimed.timestamp = Some(1);
        for source in [block(2), rehashed, retimed] {
            let error = facts
                .validate_reference(&source)
                .expect_err("another reference");
            assert_eq!(
                error,
                IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "interpreter changed the block reference",
                    false,
                )
            );
        }
    }

    /// The block states its network, so a block of another network is refused
    /// even when it holds nothing a per-transaction check could catch.
    #[test]
    fn a_block_of_another_network_is_refused_even_when_it_is_empty() {
        let elsewhere = Network::new(network().chain, "other");
        let empty = InterpretedBlock::new(&elsewhere, block(1), Vec::new()).expect("no ids");
        let error = CanonicalBlock::new(network(), empty).expect_err("another network");
        assert_eq!(
            error,
            IndexError::new(
                IndexErrorKind::NetworkMismatch,
                "block belongs to another network",
                false,
            )
        );
    }

    /// Validation binds the transactions and changes nothing else: the
    /// reference, the network, the output changes and the credits of the
    /// sealed block are the values the interpreter reported, in its order.
    #[test]
    fn validation_binds_the_transactions_and_keeps_everything_else() {
        let created = |id: &str, amount: u64| IndexedOutput {
            id: output_key(id).output,
            address: address(),
            asset: asset(),
            amount: Decimal::from(amount),
            evidence: vec![1, 2, 3],
            created_at: BlockHeight(1),
            coinbase: false,
        };
        let outputs = OutputChanges {
            created: vec![created("made-second", 7), created("made-first", 5)],
            spent: vec![output_key("spent-b"), output_key("spent-a")],
            tracked_spends: vec![output_key("tracked")],
        };
        let credits = vec![credit("withdrawal:9", "4"), credit("withdrawal:2", "6")];
        let failed = ObservationDraft {
            status: ObservationDraftStatus::Failed {
                reason: Some("reverted".into()),
            },
            movements: Vec::new(),
            ..placed("failed", 4)
        };
        let drafts = vec![placed("late", 8), failed, placed("early", 1)];
        let reported = InterpretedBlock {
            outputs: outputs.clone(),
            credits: credits.clone(),
            ..InterpretedBlock::new(&network(), block(1), drafts.clone()).expect("unique ids")
        };

        let sealed = CanonicalBlock::new(network(), reported).expect("valid facts");

        assert_eq!(sealed.outputs(), &outputs);
        assert_eq!(sealed.credits(), credits);
        assert_eq!(sealed.block(), &block(1));
        assert_eq!(sealed.network(), &network());
        let bound = sealed.transactions().iter().collect::<Vec<_>>();
        assert_eq!(bound.len(), drafts.len());
        for (canonical, draft) in bound.into_iter().zip(drafts) {
            assert_eq!(canonical.transaction_id, draft.transaction_id);
            assert_eq!(canonical.transaction_order, draft.transaction_order);
            assert_eq!(canonical.movements, draft.movements);
            assert_eq!(canonical.fee, draft.fee);
            assert_eq!(canonical.block(), &block(1));
            let expected = match draft.status {
                ObservationDraftStatus::Included => CanonicalStatus::Included { block: block(1) },
                ObservationDraftStatus::Failed { reason } => CanonicalStatus::Failed {
                    block: block(1),
                    reason,
                },
            };
            assert_eq!(canonical.status, expected);
        }
    }

    /// Two transactions of one block cannot occupy one native place, and this
    /// is the only moment anything still holds both the block's list and the
    /// reported subset to tell.
    ///
    /// The ordinal an interpreter must supply is the position in the complete
    /// block, so a repeat means it numbered what it *reported* — the rank that
    /// moves under a second, wider selection and renumbers rows beneath a
    /// cursor a caller is holding. A block whose transactions carry distinct
    /// places is accepted whether or not they are contiguous: a selection is a
    /// subset, so gaps are the normal case.
    #[test]
    fn two_transactions_of_one_block_cannot_share_a_native_place() {
        let error = refusal(interpreted(
            vec![placed("first", 3), placed("other", 5), placed("second", 3)],
            OutputChanges::default(),
        ));
        assert_eq!(
            error,
            IndexError::new(
                IndexErrorKind::InvalidBlock,
                "block contains two transactions at one native place",
                false,
            )
        );

        CanonicalBlock::new(
            network(),
            interpreted(
                vec![placed("first", 3), placed("second", 9)],
                OutputChanges::default(),
            ),
        )
        .expect("a selection is a subset, so its places may have gaps");
    }

    #[test]
    fn canonical_validation_preserves_network_status_movement_and_fee_order() {
        let own_network = network();
        let mut value = draft("-1".parse().expect("negative decimal"));
        value.transaction_id.network.name = "other".into();
        value.status = ObservationDraftStatus::Failed {
            reason: Some("failed".into()),
        };
        value.fee = Some(NetworkFee {
            asset: asset(),
            amount: "-1".parse().expect("negative decimal"),
            payer: None,
        });
        let error = value
            .clone()
            .canonical(&own_network, &block(1))
            .expect_err("a transaction of another network");
        assert_eq!(
            error,
            IndexError::new(
                IndexErrorKind::NetworkMismatch,
                "transaction belongs to another network",
                false,
            )
        );
        value.transaction_id.network = own_network.clone();
        for message in [
            "failed transaction contains movements",
            "transaction contains an invalid movement",
            "transaction contains an invalid network fee",
        ] {
            let error = value
                .clone()
                .canonical(&own_network, &block(1))
                .expect_err(message);
            assert_eq!(
                error,
                IndexError::new(IndexErrorKind::InvalidBlock, message, false)
            );
            if matches!(value.status, ObservationDraftStatus::Failed { .. }) {
                value.status = ObservationDraftStatus::Included;
            } else {
                value.movements.clear();
            }
        }
        value.status = ObservationDraftStatus::Failed {
            reason: Some("failed".into()),
        };
        value.fee.as_mut().expect("a fee").amount = Decimal::from(7_u64);
        let canonical = value
            .clone()
            .canonical(&own_network, &block(1))
            .expect("a valid failed transaction");
        assert_eq!(canonical.fee, value.fee);
        assert_eq!(
            canonical.status,
            CanonicalStatus::Failed {
                block: block(1),
                reason: Some("failed".into()),
            }
        );
        assert!(canonical.movements.is_empty());
    }
}
