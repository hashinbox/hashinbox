//! What a block says, in chain-neutral terms: `BlockCredit` (value a block
//! moves with no transaction to name it), `InterpretedBlock` (what a chain's
//! interpreter returns for one block) and `CanonicalBlock` (what the index
//! commits). Vocabulary only: no storage, no policy, no I/O.

use std::collections::BTreeSet;

use crate::{
    Address, AssetId, BlockHeight, BlockParent, BlockRef, CanonicalTransaction, Decimal,
    IndexError, IndexErrorKind, MovementId, Network, ObservationDraft, OutputChanges,
    TransactionRef,
};

/// One value change a block makes with no transaction to name it.
///
/// Post-Merge Ethereum credits a consensus-layer withdrawal straight from the
/// block body: no transaction, no receipt, no trace entry
/// (`docs/design/021` section 4.1). A fold over transactions alone therefore
/// understates such an address by every withdrawal it ever received, silently
/// and for good.
///
/// It is a record of its own rather than a synthetic
/// [`TransactionRef`](crate::TransactionRef), because that type means *the
/// chain's transaction identifier* and a reserved string prefix every reader
/// must learn is worse than a field (`docs/design/021` section 4.4). Nothing
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

/// Chain-neutral facts produced by inspecting one native block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterpretedBlock {
    /// The inspected block; must equal the source block's reference.
    pub block: BlockRef,
    /// Every transaction that touched a selected address, in block order, each
    /// carrying its own native place in the block it came from
    /// ([`ObservationDraft::transaction_order`]) rather than its place in this
    /// list.
    pub transactions: Vec<ObservationDraft>,
    /// Live-output changes (UTXO chains); empty for account chains.
    pub outputs: OutputChanges,
    /// Value the block moved to a selected address with no transaction to
    /// name it, in block order. Empty on every chain but Ethereum
    /// (`docs/CHAIN_LAYOUT.md` section 6).
    pub credits: Vec<BlockCredit>,
}

impl InterpretedBlock {
    /// Checks that interpretation did not change which block it read.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidBlock`] when the reference differs.
    pub fn validate_reference(&self, source: &BlockRef) -> Result<(), IndexError> {
        if &self.block != source {
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
/// not overlap, no transaction identity repeats, and every credit is valid and
/// uniquely identified.
///
/// It holds no raw protocol payload — those are the chain's type and megabytes
/// apiece, and the native-block cache in `fetch.rs` keeps them for the one path that
/// wants one again — and no database precondition. Which checkpoint storage
/// must hold before these facts may be written is a property of the *write*,
/// and lives on [`IndexedBatch`](crate::IndexedBatch); the facts themselves
/// are equally what a pending-block read answers from
/// (`docs/design/030 §8 (ex-032)`, the model's `Block` row).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalBlock {
    network: Network,
    block: BlockRef,
    transactions: Vec<CanonicalTransaction>,
    outputs: OutputChanges,
    credits: Vec<BlockCredit>,
}

impl CanonicalBlock {
    /// Validates `interpreted` as one network's facts: unique transactions,
    /// non-overlapping and uniquely identified output changes, network-bound
    /// movements and fees, and uniquely identified block credits.
    ///
    /// Nothing here consults storage or a node, and nothing here knows where
    /// the block will be written.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidBlock`] for invalid content, and
    /// [`IndexErrorKind::NetworkMismatch`] for a transaction of another
    /// network.
    pub fn new(network: Network, interpreted: InterpretedBlock) -> Result<Self, IndexError> {
        interpreted
            .outputs
            .validate(&network, interpreted.block.height)?;
        let mut transaction_ids = BTreeSet::<TransactionRef>::new();
        let mut orders = BTreeSet::<u64>::new();
        let mut transactions = Vec::with_capacity(interpreted.transactions.len());
        for draft in interpreted.transactions {
            if !transaction_ids.insert(draft.transaction_id.clone()) {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "block contains a duplicate transaction",
                    false,
                ));
            }
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
            transactions.push(draft.canonical(&network, &interpreted.block)?);
        }
        BlockCredit::validate_all(&interpreted.credits, &network)?;

        Ok(Self {
            network,
            block: interpreted.block,
            transactions,
            outputs: interpreted.outputs,
            credits: interpreted.credits,
        })
    }

    /// The network the block belongs to.
    #[must_use]
    pub const fn network(&self) -> &Network {
        &self.network
    }

    /// The block these facts were read from.
    #[must_use]
    pub const fn block(&self) -> &BlockRef {
        &self.block
    }

    /// The block's validated transactions, with inclusion status.
    #[must_use]
    pub fn transactions(&self) -> &[CanonicalTransaction] {
        &self.transactions
    }

    /// The block's live-output changes.
    #[must_use]
    pub const fn outputs(&self) -> &OutputChanges {
        &self.outputs
    }

    /// The value the block moved with no transaction to name it, in block
    /// order.
    #[must_use]
    pub fn credits(&self) -> &[BlockCredit] {
        &self.credits
    }

    /// Checks that this block extends `checkpoint`: height one above it,
    /// position above it, and parent linkage to exactly it.
    ///
    /// A block with no checkpoint to extend connects vacuously — it is the
    /// network's first.
    ///
    /// # Errors
    ///
    /// A retryable [`IndexErrorKind::CannotConnect`] when it does not extend
    /// the checkpoint, and [`IndexErrorKind::InvalidBlock`] when the
    /// checkpoint's height has no successor.
    pub(crate) fn connects_to(&self, checkpoint: Option<&BlockRef>) -> Result<(), IndexError> {
        if let Some(checkpoint) = checkpoint {
            let next = checkpoint.height.0.checked_add(1).ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "checkpoint height is exhausted",
                    false,
                )
            })?;
            if self.block.height != BlockHeight(next)
                || self.block.position <= checkpoint.position
                || self.block.parent.as_ref()
                    != Some(&BlockParent {
                        position: checkpoint.position,
                        hash: checkpoint.hash.clone(),
                    })
            {
                return Err(IndexError::new(
                    IndexErrorKind::CannotConnect,
                    "block does not connect to the checkpoint",
                    true,
                ));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use base::ChainName;

    use super::*;
    use crate::{
        AssetId, BlockHash, BlockPosition, CanonicalStatus, IndexedOutput, NetworkFee,
        ObservationDraftStatus, OutputId, OutputKey, ValueMovement,
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
            block: block(1),
            transactions,
            outputs,
            credits: Vec::new(),
        }
    }

    fn facts(interpreted: InterpretedBlock) -> IndexError {
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
            let error = facts(interpreted(Vec::new(), outputs));
            assert_eq!(error.kind, IndexErrorKind::InvalidBlock, "{what}");
        }
    }

    #[test]
    fn rejects_negative_movement_and_fee_amounts() {
        let negative = "-1".parse::<Decimal>().expect("negative decimal");
        let movement = facts(interpreted(
            vec![draft(negative.clone())],
            OutputChanges::default(),
        ));

        let mut fee_draft = draft(Decimal::zero());
        fee_draft.fee = Some(NetworkFee {
            asset: asset(),
            amount: negative,
            payer: Some(address()),
        });
        let fee = facts(interpreted(vec![fee_draft], OutputChanges::default()));

        assert_eq!(movement.kind, IndexErrorKind::InvalidBlock);
        assert_eq!(fee.kind, IndexErrorKind::InvalidBlock);
    }

    /// The output changes are the whole block's, so they are checked before
    /// any single transaction is: a block whose outputs overlap is invalid
    /// whichever transaction produced them.
    #[test]
    fn output_changes_are_validated_before_transactions() {
        let key = output_key("spent");
        let error = facts(interpreted(
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

    #[test]
    fn duplicate_transaction_precedes_its_validation_but_not_the_first_draft() {
        let valid = draft(Decimal::zero());
        // Same transaction id, so the two collide; the second one is also
        // invalid on its own, which is what the order decides between.
        let invalid = draft("-1".parse().expect("negative decimal"));
        for (transactions, message) in [
            (
                vec![valid.clone(), invalid.clone()],
                "block contains a duplicate transaction",
            ),
            (
                vec![invalid, valid],
                "transaction contains an invalid movement",
            ),
        ] {
            let error = facts(interpreted(transactions, OutputChanges::default()));
            assert_eq!(
                error,
                IndexError::new(IndexErrorKind::InvalidBlock, message, false)
            );
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
        let placed = |id: &str, order: u64| ObservationDraft {
            transaction_id: TransactionRef::new(network(), id),
            transaction_order: order,
            ..draft(Decimal::zero())
        };
        let error = facts(interpreted(
            vec![placed("first", 3), placed("second", 3)],
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
