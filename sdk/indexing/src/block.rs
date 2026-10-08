//! **Contract.**
//!
//! Chain-neutral block contents: transactionless credits and a block with its
//! transactions and output changes. No storage, policy, or I/O.

use crate::{
    Address, AssetId, Block, BlockHeight, BlockParent, BlockPosition, BlockRef, BoxFuture, Decimal,
    IndexError, IndexErrorKind, Network, Networked, SourceError, TransactionRef, Transactions,
};
use base::{ChainName, Identified, MovementKind, TransactionId};
use std::collections::BTreeSet;

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

/// One block's indexing facts: the inspected block with every transaction
/// that touched a selected address, its live-output changes, and the value it
/// moved with no transaction to name it.
///
/// What an interpreter returns and what a commit or a pending read consumes;
/// [`Self::validate`] is the check between them, not a conversion.
///
/// It holds no raw protocol payload — those are the chain's type and megabytes
/// apiece, and the native-block cache in `fetch.rs` keeps them for the one path
/// that wants one again — and no database precondition. Which checkpoint
/// storage must hold before these facts may be written is a property of the
/// *write*, and lives on [`IndexedBatch`](crate::IndexedBatch).
// linter:disable rust/struct-word-count -- the name design 048 fixes for a block with its outputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockWithOutputs {
    /// The inspected block in its network, with every transaction that touched
    /// a selected address, in block order, each carrying its own native place
    /// in the block it came from ([`Transaction::order`]) rather than its
    /// place in this list.
    pub block: Block<Transaction>,
    /// Live-output changes (UTXO chains); empty for account chains.
    pub outputs: OutputChanges,
    /// Value the block moved to a selected address with no transaction to
    /// name it, in block order. Empty on every chain but Ethereum.
    pub credits: Vec<BlockCredit>,
}

impl BlockWithOutputs {
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
        transactions: Vec<Transaction>,
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

    /// Checks these facts as `network`'s: the block's network, non-overlapping
    /// and uniquely identified output changes, one transaction per native
    /// place, every transaction valid and included in this very block, and
    /// uniquely identified valid credits.
    ///
    /// Transaction identifiers are already unique: [`Transactions`] refuses a
    /// repeated one when the block is assembled. Nothing here consults storage
    /// or a node.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::InvalidBlock`] for invalid content, and
    /// [`IndexErrorKind::NetworkMismatch`] for a block or a transaction of
    /// another network.
    pub fn validate(&self, network: &Network) -> Result<(), IndexError> {
        if self.block.network() != network {
            return Err(IndexError::new(
                IndexErrorKind::NetworkMismatch,
                "block belongs to another network",
                false,
            ));
        }
        let reference = self.block.reference();
        self.outputs.validate(network, reference.height)?;
        let mut orders = BTreeSet::<u64>::new();
        for transaction in self.block.transactions().iter() {
            // Two transactions of one block cannot occupy one native place, so a
            // repeated ordinal says the interpreter numbered the list it
            // *reported* rather than the list the chain executed — which is the
            // one thing `Transaction::order` may not be, and the only moment
            // anything still holds both lists to tell.
            if !orders.insert(transaction.order) {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "block contains two transactions at one native place",
                    false,
                ));
            }
            transaction.validate(network)?;
            if transaction.block() != reference {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "transaction names another block",
                    false,
                ));
            }
        }
        BlockCredit::validate_all(&self.credits, network)
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
            Some(checkpoint) if !extends(self.block.reference(), checkpoint)? => Err(
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
/// against the one before it ([`crate::read::Scan`]) and a batch checks each
/// block it carries against the line it extends
/// ([`BlockWithOutputs::connects_to`]); both ask here, so a block cannot be
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
mod block_tests {
    use base::ChainName;

    use super::*;
    use crate::{
        AssetId, BlockHash, BlockHeight, BlockPosition, IndexedOutput, NetworkFee, OutputId,
        OutputKey, Status, Transaction, TransactionRef, ValueMovement,
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

    fn draft(amount: Decimal) -> Transaction {
        Transaction {
            id: TransactionRef::new(network(), "transaction"),
            movements: vec![ValueMovement::mint(
                MovementId("movement".into()),
                asset(),
                amount,
                address(),
            )],
            fee: None,
            order: 0,
            status: Status::Included { block: block(1) },
        }
    }

    fn interpreted(transactions: Vec<Transaction>, outputs: OutputChanges) -> BlockWithOutputs {
        BlockWithOutputs {
            outputs,
            ..BlockWithOutputs::new(&network(), block(1), transactions).expect("unique ids")
        }
    }

    /// A valid draft of its own: its own identifier and its own native place.
    fn placed(id: &str, order: u64) -> Transaction {
        let mut value = draft(Decimal::zero());
        value.id = TransactionRef::new(network(), id);
        value.order = order;
        value
    }

    fn refusal(interpreted: BlockWithOutputs) -> IndexError {
        interpreted.validate(&network()).expect_err("invalid facts")
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

    /// Two transactions under one identifier are refused when the block is assembled: two
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
        let apart = Transaction {
            order: 9,
            ..valid.clone()
        };
        for transactions in [
            vec![valid.clone(), invalid.clone()],
            vec![invalid, valid.clone()],
            vec![placed("other", 1), valid.clone(), placed("third", 2), apart],
        ] {
            let error = BlockWithOutputs::new(&network(), block(1), transactions)
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

        let kept = BlockWithOutputs::new(
            &network(),
            block(1),
            vec![placed("second", 5), placed("first", 2), valid],
        )
        .expect("three identifiers");
        let ids = kept.block.transactions().iter();
        assert_eq!(
            ids.map(|draft| draft.id.id.as_str()).collect::<Vec<_>>(),
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
        let empty = BlockWithOutputs::new(&elsewhere, block(1), Vec::new()).expect("no ids");
        let error = empty.validate(&network()).expect_err("another network");
        assert_eq!(
            error,
            IndexError::new(
                IndexErrorKind::NetworkMismatch,
                "block belongs to another network",
                false,
            )
        );
    }

    /// A transaction must name the block it was read from: one carrying
    /// another block's reference is refused even when it is otherwise valid.
    #[test]
    fn a_transaction_naming_another_block_is_refused() {
        let mut stray = placed("stray", 0);
        stray.status = Status::Failed {
            block: block(2),
            reason: None,
        };
        stray.movements.clear();
        let error = refusal(interpreted(vec![stray], OutputChanges::default()));
        assert_eq!(
            error,
            IndexError::new(
                IndexErrorKind::InvalidBlock,
                "transaction names another block",
                false,
            )
        );
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

        interpreted(
            vec![placed("first", 3), placed("second", 9)],
            OutputChanges::default(),
        )
        .validate(&network())
        .expect("a selection is a subset, so its places may have gaps");
    }
}

// The vocabulary of what was observed on chain: `ValueMovement` and its
// `MovementId`, the `NetworkFee` of a transaction, and the `Transaction`
// with its `Status`. Values only; nothing here stores.

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

/// Whether a transaction succeeded, with the block that included it.
/// Confirmation is deliberately absent: it is derived from the inclusion block
/// and the current checkpoint when read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
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

/// One transaction as an interpreter reports it and storage keeps it.
///
/// One struct for every step: [`Self::validate`] is a check on it, not a
/// conversion into another type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    /// The chain's identifier and the network it belongs to.
    pub id: TransactionRef,
    /// Every stable value movement, native and token.
    pub movements: Vec<ValueMovement>,
    /// The fee, when the chain charges one.
    pub fee: Option<NetworkFee>,
    /// The transaction's native place in its block: how many transactions the
    /// chain executed before it, counting **every** one of them.
    ///
    /// **The whole block's list, never the reported subset.** An interpreter is
    /// handed the block entire and reports only the transactions that touched a
    /// selected address, so this is the one fact about block order that cannot
    /// be recovered further down: a store sees the selection and never the list.
    /// Taking the position in the complete list gives one transaction the same
    /// ordinal under every selection, which keeps a history page ordered by a
    /// function of the block alone and a held cursor stable. It is the middle
    /// component of a history cursor.
    ///
    /// Ethereum reads it from `transactionIndex`; Bitcoin and Solana take the
    /// position in the complete `tx` / `transactions` array.
    pub order: u64,
    /// Inclusion or failure, with the block.
    pub status: Status,
}

/// Identified inside its block by the chain's own identifier: the block
/// states the network, so the identifier alone tells two of its transactions
/// apart.
impl Identified for Transaction {
    type Id = TransactionId;

    fn id(&self) -> &TransactionId {
        &self.id.id
    }
}

impl Transaction {
    /// The block that included the transaction.
    #[must_use]
    pub fn block(&self) -> &BlockRef {
        match &self.status {
            Status::Included { block } | Status::Failed { block, .. } => block,
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

    /// Checks the transaction as `network`'s fact: identity first, then that a
    /// failed transaction moved nothing, then movements, then the fee.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::NetworkMismatch`] for a transaction of another
    /// network, and [`IndexErrorKind::InvalidBlock`] for invalid content.
    pub fn validate(&self, network: &Network) -> Result<(), IndexError> {
        self.validate_identity(network)?;
        if matches!(self.status, Status::Failed { .. }) && !self.movements.is_empty() {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "failed transaction contains movements",
                false,
            ));
        }
        self.validate_movements(network)?;
        if let Some(fee) = &self.fee {
            fee.validate(network)?;
        }
        Ok(())
    }

    fn validate_identity(&self, network: &Network) -> Result<(), IndexError> {
        if !self.id.belongs_to(network) {
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
        if self.id.id.as_str().is_empty() {
            return Err(IndexError::new(
                IndexErrorKind::InvalidBlock,
                "transaction has no identifier",
                false,
            ));
        }
        Ok(())
    }

    fn validate_movements(&self, network: &Network) -> Result<(), IndexError> {
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

#[cfg(test)]
mod observation_tests {
    use base::{BlockHash, BlockHeight, BlockPosition, ChainName};

    use super::*;

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

    fn draft(fee: AssetId) -> Transaction {
        Transaction {
            id: TransactionRef::new(network(), "tx"),
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
            order: 0,
            status: Status::Included { block: block() },
        }
    }

    #[test]
    fn a_transaction_touches_its_endpoints_and_fee_payer() {
        let observed = draft(AssetId::native(network().chain));
        assert_eq!(
            observed.addresses(),
            BTreeSet::from([address("a"), address("b"), address("c")])
        );
        assert!(observed.validate(&network()).is_ok());
        assert_eq!(observed.block(), &block());
    }

    #[test]
    fn failed_validation_keeps_identity_then_movements_then_fee_precedence() {
        let mut observed = draft(token());
        observed.status = Status::Failed {
            block: block(),
            reason: Some("reverted".into()),
        };
        observed.id = TransactionRef::new(network(), "");
        assert_eq!(
            observed.validate(&network()).unwrap_err().message,
            "transaction has no identifier"
        );
        observed.id = TransactionRef::new(network(), "failed");
        assert_eq!(
            observed.validate(&network()).unwrap_err().message,
            "failed transaction contains movements"
        );
        observed.movements.clear();
        assert_eq!(
            observed.validate(&network()).unwrap_err().message,
            "transaction contains an invalid network fee"
        );
        observed.fee.as_mut().unwrap().asset = AssetId::native(network().chain);
        assert!(observed.validate(&network()).is_ok());
    }

    #[test]
    fn a_network_fee_must_be_paid_in_the_native_asset() {
        let error = draft(token()).validate(&network()).unwrap_err();
        assert_eq!(error.kind, IndexErrorKind::InvalidBlock);
        assert_eq!(error.message, "transaction contains an invalid network fee");
        let native = AssetId::native(network().chain);
        assert!(draft(native).validate(&network()).is_ok());
    }
}

// Spendable outputs, for the chains that have them: `OutputId` and the
// address-qualified `OutputKey`, the `IndexedOutput` a block created, and the
// `OutputChanges` one block applies. `evidence` is opaque chain-owned bytes
// that indexing stores and never interprets.
//
// These are committed facts. Reading them back — `OutputRequest`,
// `OutputCursor`, `OutputPage` — is a repository's query and lives with the
// other queries, under `storage/`.

/// Chain-neutral identity of one transaction output.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutputId {
    /// The creating transaction.
    pub transaction: TransactionRef,
    /// The output's index within that transaction.
    pub index: u32,
}

/// Address-qualified output identity used by materialized spendable-output indexes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutputKey {
    /// The output's owner.
    pub address: Address,
    /// The output itself.
    pub output: OutputId,
}

/// A spendable output discovered in a canonical block.
///
/// `evidence` is opaque chain-owned data required by a transaction builder,
/// such as a locking script. Indexing stores it without interpreting it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedOutput {
    /// Transaction and index that created it.
    pub id: OutputId,
    /// The owner; always a selected address.
    pub address: Address,
    /// The asset it holds; must be of the network's chain.
    pub asset: AssetId,
    /// Exact amount in the asset's smallest unit.
    pub amount: Decimal,
    /// Opaque chain-owned spending data.
    pub evidence: Vec<u8>,
    /// Height of the creating block; equals the block being added.
    pub created_at: BlockHeight,
    /// Whether it is a coinbase output (subject to chain maturity rules).
    pub coinbase: bool,
}

impl IndexedOutput {
    /// The address-qualified identity used as the output index key.
    #[must_use]
    pub fn key(&self) -> OutputKey {
        OutputKey {
            address: self.address.clone(),
            output: self.id.clone(),
        }
    }
}

/// Canonical output-index changes produced while interpreting one block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputChanges {
    /// Outputs created for selected addresses.
    pub created: Vec<IndexedOutput>,
    /// Outputs of selected addresses spent in this block.
    pub spent: Vec<OutputKey>,
    /// Spends outside the current address filter are applied only if the output
    /// is already materialized. This preserves previously indexed outputs.
    pub tracked_spends: Vec<OutputKey>,
}

impl OutputChanges {
    pub(crate) fn validate(
        &self,
        network: &Network,
        block_height: BlockHeight,
    ) -> Result<(), IndexError> {
        let mut created_ids = BTreeSet::new();
        let mut created = BTreeSet::new();
        for output in &self.created {
            if !output.address.belongs_to(network)
                || !output.id.transaction.belongs_to(network)
                || output.asset.chain != network.chain
                || output.amount.validate_amount().is_err()
                || output.created_at != block_height
                || !created_ids.insert(output.id.clone())
                || !created.insert(output.key())
            {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "block contains an invalid output",
                    false,
                ));
            }
        }
        let mut spent = BTreeSet::new();
        for key in self.spent.iter().chain(&self.tracked_spends) {
            if !key.address.belongs_to(network)
                || !key.output.transaction.belongs_to(network)
                || created.contains(key)
                || !spent.insert(key)
            {
                return Err(IndexError::new(
                    IndexErrorKind::InvalidBlock,
                    "block contains overlapping or duplicate output changes",
                    false,
                ));
            }
        }
        Ok(())
    }
}

// What a chain must implement to be indexed: `NativeBlock` (a native block
// that can state its `BlockRef`), `ChainTips` (the produced and confirmed
// lines), `BlockSource` (reading blocks) and `BlockInterpreter` (turning one
// block into an `Transaction`). Four traits, no implementations, and the
// one answer an interpreter gives about its chain: `FeeSettlement`.

/// Gives generic synchronization access to a chain-native block identity.
pub trait NativeBlock: Clone + Send + Sync + 'static {
    /// The block's position, height, hash, and parent. Must be identical for
    /// the same native block every time, since synchronization compares it
    /// with [`BlockSource::canonical_at`] before and after interpretation.
    fn block_ref(&self) -> BlockRef;
}

/// The two lines a chain states about itself: how far it has produced, and
/// how far it will never take back.
///
/// Both are read once per pass. The distance between them is the whole of
/// [`crate::Indexer`]'s pending blocks: blocks at or below the confirmed tip
/// are committed to durable storage, blocks above it are held in memory.
pub trait ChainTips: Send + Sync {
    /// The newest produced block the source considers canonical. A pass
    /// reads up to this tip, never beyond it.
    fn tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>>;

    /// The newest block this source will state is irreversible. Never above
    /// [`Self::tip`], and never moving backwards.
    ///
    /// This is the line the durable store commits at, so no stored row is ever
    /// undone. There is deliberately no default implementation, because the
    /// answer is a property of the chain's consensus and each one must state
    /// its own: Ethereum asks its node for the `finalized` block, Solana
    /// already reads only rooted blocks and returns its own tip, and Bitcoin,
    /// which has no finality, subtracts a configured depth and accepts a
    /// probabilistic line.
    fn confirmed_tip<'a>(&'a self) -> BoxFuture<'a, Result<BlockRef, SourceError>>;
}

/// Fetches canonical chain data. Concrete RPC methods remain in the chain crate.
///
/// Every error is a [`SourceError`]; its `retryable` flag decides whether the
/// driver tries the pass again or quarantines
/// that one network.
pub trait BlockSource: ChainTips {
    /// The chain-native block this source fetches.
    type Block: NativeBlock;

    /// Returns at most `limit` actual produced blocks in the inclusive range,
    /// in strictly increasing position order. Positions without a produced
    /// block (Solana's skipped slots) are omitted, not returned empty.
    fn blocks<'a>(
        &'a self,
        start: BlockPosition,
        end: BlockPosition,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Self::Block>, SourceError>>;

    /// Returns the complete canonical reference at one native position, or
    /// `None` when no produced block is canonical there. Used to detect reorgs.
    fn canonical_at<'a>(
        &'a self,
        position: BlockPosition,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, SourceError>>;
}

/// Where a chain settles its network fee, relative to the movements its
/// interpreter emits.
///
/// The distinction is not cosmetic and it is not guessable. On a UTXO chain
/// the interpreter emits every spent input and every created output, so
/// `Σ inputs − Σ outputs` has already paid the fee; subtracting
/// [`NetworkFee`](crate::NetworkFee) again would charge it twice. On an
/// account chain the fee is charged outside the transferred value and has to
/// be applied, to its payer and in the fee asset only — which is why a token
/// holding never pays gas, and the same address's native holding always does.
///
/// **A fact about the chain, stated by its interpreter**
/// ([`BlockInterpreter::settlement`]) and carried to the store with every
/// batch ([`crate::IndexedBatch::settlement`]). It is declared here, beside the
/// trait that answers it, because nothing about it is a query: a store reads
/// it when it writes, and whatever it folds afterwards is its own business.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeeSettlement {
    /// Already inside the movements: nothing may apply it again.
    Embedded,
    /// Charged beside them: the payer is debited, in the fee asset only.
    Charged,
}

/// Converts a chain-native block into relevant canonical changes.
///
/// Pure and synchronous: it sees one block and the addresses active at its
/// position, and never touches storage or the network.
pub trait BlockInterpreter: Send + Sync {
    /// The chain-native block this interpreter reads; matches the source's.
    type Block: NativeBlock;

    /// Extracts the transactions and output changes that touch `addresses`.
    ///
    /// The reference of the returned [`BlockWithOutputs::block`] must equal
    /// [`NativeBlock::block_ref`], and its network must be the one being
    /// synchronized; synchronization rejects anything else as
    /// [`IndexErrorKind::InvalidBlock`](crate::IndexErrorKind::InvalidBlock) or
    /// [`IndexErrorKind::NetworkMismatch`](crate::IndexErrorKind::NetworkMismatch).
    /// The output is assembled with [`BlockWithOutputs::new`], which refuses
    /// two transactions under one identifier.
    /// Malformed native content is also `InvalidBlock` (terminal).
    fn inspect(
        &self,
        block: &Self::Block,
        addresses: &[Address],
    ) -> Result<BlockWithOutputs, IndexError>;

    /// Where this chain's network fee already sits, relative to the movements
    /// [`Self::inspect`] emits.
    ///
    /// There is deliberately no default: a balance fold that guesses wrong is
    /// wrong by exactly one fee on every transaction the holding paid for.
    /// The interpreter answers it because the interpreter is what produced
    /// both the movements and the [`NetworkFee`](crate::NetworkFee), so it is
    /// the one place that knows whether they overlap.
    fn settlement(&self) -> FeeSettlement;
}

// What a `base::Network` must admit before it may be used as an index
// network: the chain-family check every component makes, and the strict
// conversion of caller addresses into a chain's native address set.
//
// Two free functions rather than a trait. Both take the network as their
// first argument because `IndexError` lives here and `Network` lives in
// `base`, so neither can be an inherent method. A one-method-per-caller trait
// with exactly one implementation bought method syntax and nothing else, and
// `rules."rust/free-function".mode = "receiver"` was checked against this
// shape and does not forbid it.

/// Accepts `network` for a component of the `chain` family.
///
/// # Errors
///
/// [`IndexErrorKind::NetworkMismatch`] for another chain or a blank network.
pub fn require_chain(network: &Network, chain: &ChainName) -> Result<(), IndexError> {
    if network.chain != *chain {
        return Err(IndexError::new(
            IndexErrorKind::NetworkMismatch,
            format!("network chain {} is not {chain}", network.chain),
            false,
        ));
    }
    if network.name.trim().is_empty() {
        return Err(IndexError::new(
            IndexErrorKind::NetworkMismatch,
            format!("{chain} network name must not be empty"),
            false,
        ));
    }
    Ok(())
}

/// Converts caller-selected `addresses` into `network`'s native address set,
/// strictly (exact canonical text), so an interpreter can test membership
/// natively.
///
/// # Errors
///
/// [`IndexErrorKind::NetworkMismatch`] when an address belongs to another
/// network and [`IndexErrorKind::InvalidRequest`] when `A` rejects its text.
pub fn select_addresses<A>(
    network: &Network,
    addresses: &[Address],
) -> Result<BTreeSet<A>, IndexError>
where
    A: for<'a> TryFrom<&'a Address> + Ord,
{
    let foreign = || {
        IndexError::new(
            IndexErrorKind::NetworkMismatch,
            format!(
                "address filter belongs to a network other than {}",
                network.chain
            ),
            false,
        )
    };
    let malformed = || {
        IndexError::new(
            IndexErrorKind::InvalidRequest,
            format!(
                "address filter is not a canonical {} address",
                network.chain
            ),
            false,
        )
    };
    addresses
        .iter()
        .map(|address| {
            if !address.belongs_to(network) {
                return Err(foreign());
            }
            A::try_from(address).map_err(|_| malformed())
        })
        .collect()
}

#[cfg(test)]
mod network_tests {

    use super::*;

    fn network(chain: &str, network: &str) -> Network {
        Network::new(ChainName::new(chain), network)
    }

    /// A native address double: canonical decimal text only.
    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct Number(u32);

    impl TryFrom<&Address> for Number {
        type Error = ();

        fn try_from(address: &Address) -> Result<Self, ()> {
            let number = address.value.parse::<u32>().map_err(|_| ())?;
            (number.to_string() == address.value)
                .then_some(Self(number))
                .ok_or(())
        }
    }

    #[test]
    fn required_chain_rejects_foreign_chain_and_blank_network() {
        let chain = ChainName::new("test");
        assert!(require_chain(&network("test", "mainnet"), &chain).is_ok());
        for rejected in [network("other", "mainnet"), network("test", " ")] {
            let error = require_chain(&rejected, &chain).expect_err("network must be rejected");
            assert_eq!(error.kind, IndexErrorKind::NetworkMismatch);
            assert!(!error.retryable);
        }
    }

    #[test]
    fn selected_addresses_parse_into_a_native_set() {
        let network = network("test", "mainnet");
        let addresses = [
            Address::new(network.clone(), "7"),
            Address::new(network.clone(), "3"),
            Address::new(network.clone(), "7"),
        ];

        let selected = select_addresses::<Number>(&network, &addresses).expect("valid selection");

        assert_eq!(
            selected.into_iter().collect::<Vec<_>>(),
            [Number(3), Number(7)]
        );
    }

    #[test]
    fn selected_addresses_reject_foreign_network_and_non_canonical_text() {
        let network = network("test", "mainnet");
        for (address, kind) in [
            (
                Address::new(self::network("test", "testnet"), "1"),
                IndexErrorKind::NetworkMismatch,
            ),
            (
                Address::new(network.clone(), "not-a-number"),
                IndexErrorKind::InvalidRequest,
            ),
            (
                Address::new(network.clone(), "07"),
                IndexErrorKind::InvalidRequest,
            ),
        ] {
            let error = select_addresses::<Number>(&network, &[address])
                .expect_err("selection must be rejected");
            assert_eq!(error.kind, kind);
            assert!(!error.retryable);
        }
    }
}
