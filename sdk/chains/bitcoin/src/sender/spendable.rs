//! [`Spendable`]: the store-backed [`UtxoSource`].
//!
//! The index materializes UTXOs as `indexing::IndexedOutput`s. This source
//! reads every page at one checkpoint, folds the blocks the index still holds
//! above that line, checks each output really belongs to the address, and
//! skips immature coinbase outputs.

use std::{collections::BTreeSet, sync::Arc};

use base::{BlockRef, Networked};
use indexing::{
    AssetId, BlockWithOutputs, IndexError, IndexErrorKind, IndexedOutput, OutputCursor, OutputKey,
    OutputRequest, OutputStore, Pending,
};
use wallets::{FutureResult, WalletError, WalletErrorKind};

use super::UtxoSource;
use crate::native::{
    Address, CHAIN_NAME, Network, Satoshi, TransactionId,
    block::{COINBASE_MATURITY, Outpoint, Output, Utxo},
};

const PAGE_SIZE: usize = 256;

/// Store-backed input source: lists live outputs, folds the pending blocks
/// above the confirmed line (change spendable at depth one), and passes on
/// the store's refusal while an address is still catching up
/// ([`indexing::Inspection::spendable`]).
pub struct Spendable {
    store: Arc<dyn OutputStore>,
    pending: Arc<dyn Pending>,
    network: Network,
}

/// The address one read is for, in each form an indexed output of it is
/// checked against.
struct Owner<'a> {
    address: &'a Address,
    canonical: base::Address,
    script: Vec<u8>,
}

impl Spendable {
    /// The live outputs `store` holds for `network`, completed by what
    /// `pending` holds above the store's line. `pending` must be the index
    /// that commits into `store` for the same network.
    #[must_use]
    pub fn new(store: Arc<dyn OutputStore>, pending: Arc<dyn Pending>, network: Network) -> Self {
        Self {
            store,
            pending,
            network,
        }
    }

    /// Every spendable output of `addresses`, all read at one checkpoint and
    /// folded over one sample of pending blocks.
    ///
    /// The sample is taken **before** the store is read and narrowed by the
    /// store's own checkpoint ([`indexing::Sample::above`]), so a commit
    /// landing in between is counted once.
    ///
    /// # Errors
    ///
    /// A retryable `Conflict` when the checkpoint moves during the read, and
    /// a terminal `Store` error for outputs that do not belong to the address.
    async fn read(&self, addresses: &[Address]) -> Result<Vec<Utxo>, IndexError> {
        let sample = self.pending.sampled()?;
        let mut checkpoint = None;
        let mut rows = Vec::new();
        for address in addresses {
            let owner = Owner {
                address,
                canonical: address.canonical(&self.network),
                script: address
                    .script_pubkey(self.network)
                    .map_err(|error| corrupt(error.message))?
                    .into_bytes(),
            };
            let listed = self.list(&owner, &mut checkpoint).await?;
            rows.push((owner, listed));
        }
        let Some(line) = checkpoint else {
            return Err(IndexError::new(
                IndexErrorKind::Conflict,
                "Bitcoin output lookup requires an address and a canonical checkpoint",
                true,
            ));
        };
        let pending = sample.above(&Line(line.clone()));
        let mut seen = BTreeSet::new();
        let mut outputs = Vec::new();
        for (owner, mut listed) in rows {
            let changes = LiveChanges::fold(&owner.canonical, &pending);
            listed.retain(|output| !changes.spent.contains(&output.key()));
            listed.extend(changes.created);
            for output in listed {
                outputs.extend(Self::utxo(output, &owner, &line, &mut seen)?);
            }
        }
        Ok(outputs)
    }

    /// Every stored live output of `owner`, paged at one checkpoint.
    async fn list(
        &self,
        owner: &Owner<'_>,
        checkpoint: &mut Option<BlockRef>,
    ) -> Result<Vec<IndexedOutput>, IndexError> {
        let mut after: Option<OutputCursor> = None;
        let mut outputs = Vec::new();
        loop {
            let request = OutputRequest {
                network: self.network.canonical(),
                address: owner.canonical.clone(),
                after: after.clone(),
                limit: PAGE_SIZE,
            };
            let page = self.store.list(request).await?;
            let current = page.checkpoint.as_ref().ok_or_else(|| {
                IndexError::new(
                    IndexErrorKind::Conflict,
                    "indexed outputs have no canonical checkpoint",
                    true,
                )
            })?;
            if checkpoint.get_or_insert_with(|| current.clone()) != current {
                return Err(IndexError::new(
                    IndexErrorKind::Conflict,
                    "indexed output checkpoint changed while loading Bitcoin outputs",
                    true,
                ));
            }
            outputs.extend(page.outputs);
            let Some(next) = page.next else {
                return Ok(outputs);
            };
            if next.checkpoint != page.checkpoint
                || after.as_ref() == Some(&next)
                || next.position.is_empty()
            {
                return Err(corrupt(
                    "indexed output query returned an invalid pagination cursor",
                ));
            }
            after = Some(next);
        }
    }

    /// Rejects an indexed output that is not `owner.address`'s native coin.
    fn check_owner(output: &IndexedOutput, owner: &Owner<'_>) -> Result<(), IndexError> {
        if output.address != owner.canonical
            || output.asset != AssetId::native(CHAIN_NAME)
            || !output.id.transaction.belongs_to(&owner.canonical.network)
        {
            return Err(corrupt(
                "indexed output does not belong to the requested Bitcoin address and asset",
            ));
        }
        if output.evidence != owner.script {
            return Err(corrupt(
                "indexed output locking script does not match its Bitcoin address",
            ));
        }
        Ok(())
    }

    /// How deep `output` is at `checkpoint`.
    ///
    /// An output created above the confirmed line counts as one confirmation:
    /// the checkpoint *is* that line, and the pending fold contributes
    /// outputs from blocks beyond it. Saturating is the conservative
    /// direction for the one rule that reads this depth -- coinbase maturity
    /// -- because such an output is at most the confirmation depth deep and
    /// so is immature either way.
    fn depth(output: &IndexedOutput, checkpoint: &BlockRef) -> Result<u64, IndexError> {
        checkpoint
            .height
            .0
            .saturating_sub(output.created_at.0)
            .checked_add(1)
            .ok_or_else(|| corrupt("indexed output height is exhausted"))
    }

    /// `output` as a UTXO of `owner.address`; `None` while it is an immature coinbase.
    fn utxo(
        output: IndexedOutput,
        owner: &Owner<'_>,
        checkpoint: &BlockRef,
        seen: &mut BTreeSet<Outpoint>,
    ) -> Result<Option<Utxo>, IndexError> {
        Self::check_owner(&output, owner)?;
        let transaction_id = output
            .id
            .transaction
            .id
            .as_str()
            .parse::<TransactionId>()
            .map_err(|_| corrupt("indexed output has an invalid Bitcoin transaction ID"))?;
        // Indexing stores satoshis (0 decimals), never BTC.
        let value = output
            .amount
            .to_atomic_u64(0)
            .map_err(|error| corrupt(error.to_string()))?;
        if output.coinbase && Self::depth(&output, checkpoint)? < COINBASE_MATURITY {
            return Ok(None);
        }
        let outpoint = Outpoint {
            transaction_id,
            output_index: output.id.index,
        };
        if !seen.insert(outpoint) {
            return Err(corrupt("indexed output is duplicated"));
        }
        Ok(Some(Utxo {
            outpoint,
            output: Output {
                value: Satoshi::from_atomic(value),
                script_pubkey: output.evidence,
                address: Some(owner.address.clone()),
            },
        }))
    }
}

impl UtxoSource for Spendable {
    fn utxos<'a>(&'a self, addresses: &'a [Address]) -> FutureResult<'a, Vec<Utxo>> {
        Box::pin(async move {
            self.read(addresses)
                .await
                .map_err(|error| WalletError::new(WalletErrorKind::Unavailable, error.message))
        })
    }
}

/// The checkpoint every page of one read was listed at: the store's own
/// answer, so the pending sample is narrowed by the line the rows came from.
struct Line(BlockRef);

impl indexing::Checkpointed for Line {
    fn checkpoint(&self) -> Option<&BlockRef> {
        Some(&self.0)
    }
}

/// What the blocks above the line change about one address's live outputs.
///
/// Folded over the confirmed rows rather than filtering them by depth. A
/// read-time depth filter would be actively wrong: a spend of an old output
/// is already applied while the change it created would be hidden, and the
/// outbox infers "spent" from absence, so the wallet would free the input,
/// see no change, and report a hole. Both halves move together here because
/// they come from the same blocks.
struct LiveChanges {
    /// Outputs those blocks created for the address and no later one spent.
    created: Vec<IndexedOutput>,
    /// Keys those blocks spent, whether or not the store holds them.
    spent: BTreeSet<OutputKey>,
}

impl LiveChanges {
    /// The net effect of `blocks` on `address`'s live outputs.
    fn fold(address: &base::Address, blocks: &[BlockWithOutputs]) -> Self {
        let mut created = Vec::new();
        let mut spent = BTreeSet::new();
        for changes in blocks.iter().map(|facts| &facts.outputs) {
            for key in changes.spent.iter().chain(&changes.tracked_spends) {
                spent.insert(key.clone());
            }
            created.retain(|output: &IndexedOutput| !spent.contains(&output.key()));
            created.extend(
                changes
                    .created
                    .iter()
                    .filter(|output| output.address == *address)
                    .cloned(),
            );
        }
        Self { created, spent }
    }
}

/// Indexed data that contradicts the chain: retrying cannot fix it.
fn corrupt(message: impl Into<String>) -> IndexError {
    IndexError::new(IndexErrorKind::Store, message, false)
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use base::{BlockHash, BlockHeight, BlockPosition, BoxFuture, Decimal};
    use futures_executor::block_on;
    use indexing::{OutputId, OutputPage};

    use super::*;
    use crate::native::{AddressType, KeyPair};

    /// One expected cursor and the page it gets.
    type Scripted = (Option<OutputCursor>, Result<OutputPage, IndexError>);

    /// Replies with scripted pages in order, checking each request's cursor.
    struct Pages(Mutex<VecDeque<Scripted>>);

    impl OutputStore for Pages {
        fn list<'a>(
            &'a self,
            request: OutputRequest,
        ) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
            let (after, page) = self.0.lock().unwrap().pop_front().expect("unexpected read");
            assert_eq!(request.after, after);
            Box::pin(async { page })
        }
    }

    /// The blocks an index holds above its line.
    struct Held(Vec<BlockWithOutputs>);

    impl Pending for Held {
        fn sampled(&self) -> Result<indexing::Sample, IndexError> {
            Ok(indexing::Sample::new(self.0.clone(), 0))
        }
    }

    fn utxos(pages: Vec<Scripted>) -> Spendable {
        held(pages, Vec::new())
    }

    fn held(pages: Vec<Scripted>, blocks: Vec<BlockWithOutputs>) -> Spendable {
        Spendable::new(
            Arc::new(Pages(Mutex::new(pages.into()))),
            Arc::new(Held(blocks)),
            Network::Regtest,
        )
    }

    /// A pending block at `height` that spends `spent` and creates `created`.
    fn pending(
        height: u64,
        spent: Vec<OutputKey>,
        created: Vec<IndexedOutput>,
    ) -> BlockWithOutputs {
        BlockWithOutputs {
            outputs: indexing::OutputChanges {
                created,
                spent,
                tracked_spends: Vec::new(),
            },
            ..BlockWithOutputs::new(
                &Network::Regtest.canonical(),
                checkpoint(height),
                Vec::new(),
            )
            .expect("an empty block")
        }
    }

    fn address(byte: u8) -> Address {
        KeyPair::import(crypto::SecretBytes::new([byte; 32]))
            .unwrap()
            .address(Network::Regtest, AddressType::SegwitV0)
    }

    fn checkpoint(height: u64) -> BlockRef {
        BlockRef {
            position: BlockPosition(height),
            height: BlockHeight(height),
            hash: BlockHash(vec![height as u8]),
            parent: None,
            timestamp: None,
        }
    }

    fn output(address: &Address, created: u64, coinbase: bool) -> IndexedOutput {
        let network = Network::Regtest.canonical();
        IndexedOutput {
            id: OutputId {
                transaction: TransactionId([7; 32]).canonical(&network),
                index: 3,
            },
            address: address.canonical(&network),
            asset: AssetId::native(CHAIN_NAME),
            amount: Decimal::from(100_000_u64),
            evidence: address
                .script_pubkey(Network::Regtest)
                .unwrap()
                .into_bytes(),
            created_at: BlockHeight(created),
            coinbase,
        }
    }

    fn page(
        checkpoint: BlockRef,
        outputs: Vec<IndexedOutput>,
        next: Option<OutputCursor>,
    ) -> Result<OutputPage, IndexError> {
        Ok(OutputPage {
            checkpoint: Some(checkpoint),
            outputs,
            next,
        })
    }

    #[test]
    fn output_validation_order_precedes_duplicate_claims() {
        let owner = address(1);
        let messages = [
            "indexed output does not belong to the requested Bitcoin address and asset",
            "indexed output locking script does not match its Bitcoin address",
            "indexed output has an invalid Bitcoin transaction ID",
            "amount has more than 0 fractional digits",
        ];
        for (stage, message) in messages.into_iter().enumerate() {
            let mut invalid = output(&owner, 11, false);
            if stage == 0 {
                invalid.asset.asset = "token".to_owned();
            }
            if stage <= 1 {
                invalid.evidence.clear();
            }
            if stage <= 2 {
                invalid.id.transaction.id = "invalid".to_owned().into();
            }
            if stage <= 3 {
                invalid.amount = "1.1".parse().unwrap();
            }
            let reader = utxos(vec![(None, page(checkpoint(10), vec![invalid], None))]);
            let error = block_on(reader.read(std::slice::from_ref(&owner))).unwrap_err();
            assert_eq!((error.message.as_str(), error.retryable), (message, false));
        }
        let reader = utxos(vec![(
            None,
            page(checkpoint(10), vec![output(&owner, 1, false); 2], None),
        )]);
        let error = block_on(reader.read(std::slice::from_ref(&owner))).unwrap_err();
        assert_eq!(error.message, "indexed output is duplicated");
    }

    /// A pending block's spend hides the stored output and its change is
    /// offered: change is spendable at depth one.
    #[test]
    fn pending_blocks_fold_spends_and_change_over_the_stored_rows() {
        let owner = address(1);
        let funding = output(&owner, 5, false);
        let mut change = output(&owner, 11, false);
        change.id.transaction = TransactionId([8; 32]).canonical(&Network::Regtest.canonical());
        let reader = held(
            vec![(None, page(checkpoint(10), vec![funding.clone()], None))],
            vec![pending(11, vec![funding.key()], vec![change])],
        );

        let read = block_on(reader.read(std::slice::from_ref(&owner))).expect("folded outputs");

        assert_eq!(read.len(), 1);
        assert_eq!(read[0].outpoint.transaction_id, TransactionId([8; 32]));
    }

    /// A block the store committed after the sample was taken is narrowed out
    /// by the store's own line: its change is listed once, not twice.
    #[test]
    fn a_block_committed_after_the_sample_is_counted_once() {
        let owner = address(1);
        let change = output(&owner, 11, false);
        let reader = held(
            vec![(None, page(checkpoint(11), vec![change.clone()], None))],
            vec![pending(11, Vec::new(), vec![change])],
        );

        let read = block_on(reader.read(std::slice::from_ref(&owner))).expect("one change");

        assert_eq!(read.len(), 1);
    }

    /// An output created above the confirmed line is what the pending overlay
    /// contributes, and it is spendable at depth one: the page's checkpoint is
    /// the line, so the output's height is legitimately above it.
    #[test]
    fn an_output_created_above_the_page_checkpoint_is_spendable() {
        let owner = address(1);
        let change = output(&owner, 11, false);
        let reader = utxos(vec![(None, page(checkpoint(10), vec![change], None))]);

        let read = block_on(reader.read(std::slice::from_ref(&owner))).expect("pending change");

        assert_eq!(read.len(), 1);
    }

    /// The same output as a coinbase is still immature: an unmined-depth
    /// coinbase counts as one confirmation, far below the maturity rule.
    #[test]
    fn a_coinbase_output_above_the_page_checkpoint_stays_immature() {
        let owner = address(1);
        let coinbase = output(&owner, 11, true);
        let reader = utxos(vec![(None, page(checkpoint(10), vec![coinbase], None))]);

        let read = block_on(reader.read(std::slice::from_ref(&owner))).expect("immature coinbase");

        assert!(read.is_empty());
    }

    #[test]
    fn store_errors_keep_message_and_retryability() {
        for retryable in [false, true] {
            let expected =
                IndexError::new(IndexErrorKind::Store, " store failed\ncontext ", retryable);
            let reader = utxos(vec![(None, Err(expected.clone()))]);
            let error = block_on(reader.read(&[address(1)])).unwrap_err();
            assert_eq!(error, expected);
        }
    }

    #[test]
    fn pages_and_addresses_must_keep_one_checkpoint() {
        let first = checkpoint(10);
        let mut reorg = first.clone();
        reorg.hash = BlockHash(vec![99]);
        let cursor = OutputCursor {
            checkpoint: Some(first.clone()),
            position: vec![1],
        };
        for last in [first.clone(), checkpoint(11), reorg] {
            let reader = utxos(vec![
                (None, page(first.clone(), Vec::new(), Some(cursor.clone()))),
                (Some(cursor.clone()), page(first.clone(), Vec::new(), None)),
                (None, page(last.clone(), Vec::new(), None)),
            ]);
            let result = block_on(reader.read(&[address(1), address(2)]));
            if last == first {
                assert!(result.unwrap().is_empty());
                continue;
            }
            let error = result.unwrap_err();
            assert!(error.retryable);
            assert_eq!(
                error.message,
                "indexed output checkpoint changed while loading Bitcoin outputs"
            );
        }
    }

    #[test]
    fn cursors_must_match_the_page_and_advance() {
        let current = checkpoint(10);
        for cursor in [
            OutputCursor {
                checkpoint: Some(checkpoint(11)),
                position: vec![1],
            },
            OutputCursor {
                checkpoint: Some(current.clone()),
                position: Vec::new(),
            },
        ] {
            let reader = utxos(vec![(
                None,
                page(current.clone(), Vec::new(), Some(cursor)),
            )]);
            let error = block_on(reader.read(&[address(1)])).unwrap_err();
            assert_eq!(
                error.message,
                "indexed output query returned an invalid pagination cursor"
            );
        }
    }

    #[test]
    fn immature_coinbase_outputs_are_skipped_before_duplicate_tracking() {
        let owner = address(1);
        let mature = output(&owner, 1, true);
        let immature = output(&owner, 2, true);
        let reader = utxos(vec![(
            None,
            page(
                checkpoint(100),
                vec![immature.clone(), immature, mature],
                None,
            ),
        )]);
        let spendable = block_on(reader.read(std::slice::from_ref(&owner))).unwrap();
        assert_eq!(spendable.len(), 1);
        assert_eq!(spendable[0].output.value, Satoshi::from_atomic(100_000));
        assert_eq!(spendable[0].output.address, Some(owner));
    }
}
