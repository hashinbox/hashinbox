//! The spendable outputs of wallet addresses, read from the index.
//!
//! The index materializes UTXOs as `indexing::IndexedOutput`s. This view
//! reads every page at one checkpoint, checks each output really belongs to
//! the address, and skips immature coinbase outputs.

use std::{collections::BTreeSet, sync::Arc};

use base::{BlockRef, Networked};
use indexing::{
    AssetId, IndexError, IndexErrorKind, IndexedOutput, OutputCursor, OutputRequest, OutputStore,
};

use crate::native::{
    Address, CHAIN_NAME, Network, Satoshi, TransactionId,
    block::{COINBASE_MATURITY, Outpoint, Output, Utxo},
};

const PAGE_SIZE: usize = 256;

/// UTXOs of one or more addresses at one index checkpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Utxos {
    /// The checkpoint every page was read at.
    pub(crate) checkpoint: BlockRef,
    /// Mature, unique outputs in index order.
    pub(crate) outputs: Vec<Utxo>,
}

/// Reads the index's outputs for one network and keeps only spendable UTXOs.
pub(crate) struct UtxoReader {
    network: Network,
    outputs: Arc<dyn OutputStore>,
}

/// The address one read is for, in each form an indexed output of it is
/// checked against.
struct Owner<'a> {
    address: &'a Address,
    canonical: base::Address,
    script: Vec<u8>,
}

impl UtxoReader {
    pub(crate) fn new(network: Network, outputs: Arc<dyn OutputStore>) -> Self {
        Self { network, outputs }
    }

    /// Every spendable output of `addresses`, all read at one checkpoint.
    ///
    /// # Errors
    ///
    /// A retryable `Conflict` when the checkpoint moves during the read, and
    /// a terminal `Store` error for outputs that do not belong to the address.
    pub(crate) async fn read(&self, addresses: &[Address]) -> Result<Utxos, IndexError> {
        let mut checkpoint = None;
        let mut seen = BTreeSet::new();
        let mut outputs = Vec::new();
        for address in addresses {
            let owner = Owner {
                address,
                canonical: address.canonical(&self.network),
                script: address
                    .script_pubkey(self.network)
                    .map_err(|error| corrupt(error.message))?
                    .into_bytes(),
            };
            outputs.extend(self.address(&owner, &mut checkpoint, &mut seen).await?);
        }
        let checkpoint = checkpoint.ok_or_else(|| {
            IndexError::new(
                IndexErrorKind::Conflict,
                "Bitcoin output lookup requires an address and a canonical checkpoint",
                true,
            )
        })?;
        Ok(Utxos {
            checkpoint,
            outputs,
        })
    }

    async fn address(
        &self,
        owner: &Owner<'_>,
        checkpoint: &mut Option<BlockRef>,
        seen: &mut BTreeSet<Outpoint>,
    ) -> Result<Vec<Utxo>, IndexError> {
        let mut after: Option<OutputCursor> = None;
        let mut utxos = Vec::new();
        loop {
            let request = OutputRequest {
                network: self.network.canonical(),
                address: owner.canonical.clone(),
                after: after.clone(),
                limit: PAGE_SIZE,
            };
            let page = self.outputs.list(request).await?;
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
            for output in page.outputs {
                utxos.extend(self.utxo(output, owner, current, seen)?);
            }
            let Some(next) = page.next else {
                return Ok(utxos);
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
    /// the page's checkpoint *is* that line, and the overlay contributes
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
        &self,
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

    fn utxos(pages: Vec<Scripted>) -> UtxoReader {
        UtxoReader::new(Network::Regtest, Arc::new(Pages(Mutex::new(pages.into()))))
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

    /// An output created above the confirmed line is what the pending overlay
    /// contributes, and it is spendable at depth one: the page's checkpoint is
    /// the line, so the output's height is legitimately above it.
    #[test]
    fn an_output_created_above_the_page_checkpoint_is_spendable() {
        let owner = address(1);
        let change = output(&owner, 11, false);
        let reader = utxos(vec![(None, page(checkpoint(10), vec![change], None))]);

        let read = block_on(reader.read(std::slice::from_ref(&owner))).expect("pending change");

        assert_eq!(read.outputs.len(), 1);
        assert_eq!(read.checkpoint, checkpoint(10));
    }

    /// The same output as a coinbase is still immature: an unmined-depth
    /// coinbase counts as one confirmation, far below the maturity rule.
    #[test]
    fn a_coinbase_output_above_the_page_checkpoint_stays_immature() {
        let owner = address(1);
        let coinbase = output(&owner, 11, true);
        let reader = utxos(vec![(None, page(checkpoint(10), vec![coinbase], None))]);

        let read = block_on(reader.read(std::slice::from_ref(&owner))).expect("immature coinbase");

        assert!(read.outputs.is_empty());
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
            revision: None,
        };
        for last in [first.clone(), checkpoint(11), reorg] {
            let reader = utxos(vec![
                (None, page(first.clone(), Vec::new(), Some(cursor.clone()))),
                (Some(cursor.clone()), page(first.clone(), Vec::new(), None)),
                (None, page(last.clone(), Vec::new(), None)),
            ]);
            let result = block_on(reader.read(&[address(1), address(2)]));
            if last == first {
                assert_eq!(result.unwrap().checkpoint, first);
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
                revision: None,
            },
            OutputCursor {
                checkpoint: Some(current.clone()),
                position: Vec::new(),
                revision: None,
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
        assert_eq!(spendable.checkpoint, checkpoint(100));
        assert_eq!(spendable.outputs.len(), 1);
        assert_eq!(
            spendable.outputs[0].output.value,
            Satoshi::from_atomic(100_000)
        );
        assert_eq!(spendable.outputs[0].output.address, Some(owner));
    }
}
