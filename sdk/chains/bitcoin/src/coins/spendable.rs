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
