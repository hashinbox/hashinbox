//! A scripted node and index for adapter unit tests: fee estimates,
//! mempool preflight, submission, lookups, indexed outputs, and checkpoint,
//! recording every call.

use std::{
    collections::{BTreeSet, VecDeque},
    sync::{Arc, Mutex},
};

use base::{BlockHash, BlockHeight, BlockPosition, BlockRef, BoxFuture, Decimal, SecretBytes};
use indexing::{
    AssetId, IndexError, IndexedOutput, OutputId, OutputPage, OutputRequest, OutputStore,
    TransactionPage,
};

use super::{Item, Outbox, UtxoReader};
use crate::native::{
    Address, AddressType, CHAIN_NAME, Error, KeyPair, Network, Satoshi, SignedTransaction,
    TransactionId,
    rpc::Submitter,
    transaction::{Acceptance, FeeRate, Preflight, Transfer},
};
use crate::{WalletConfig, WalletFactory};

/// What one broadcast does; once the queue is empty, [`Node::broadcast`]
/// decides.
pub(crate) enum Action {
    Fail(Error),
    /// Accepts once the gate is notified.
    Gate(Arc<tokio::sync::Notify>),
}

/// Every reply the double gives, and the calls it saw.
pub(crate) struct Node {
    /// Calls in order: `fees`, an address per output read, `preflight`,
    /// `broadcast`, `known`.
    pub(crate) events: Mutex<Vec<String>>,
    pub(crate) fee: Result<FeeRate, Error>,
    /// Output `i` of address `a` is `(txid [i + 1; 32], 0)`.
    pub(crate) outputs: Result<Vec<(Address, u64)>, IndexError>,
    /// Output numbers (`i + 1`) the index shows spent.
    pub(crate) spent: Mutex<BTreeSet<u8>>,
    pub(crate) acceptance: Result<Acceptance, Error>,
    pub(crate) actions: Mutex<VecDeque<Action>>,
    /// Once `actions` is empty: `None` echoes the submitted id.
    pub(crate) broadcast: Option<Error>,
    pub(crate) known: Mutex<VecDeque<Result<bool, Error>>>,
    pub(crate) submitted: Mutex<Vec<SignedTransaction>>,
}

impl Default for Node {
    fn default() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            fee: Ok(FeeRate::from_sat_per_kvb(1_000)),
            outputs: Ok(Vec::new()),
            spent: Mutex::new(BTreeSet::new()),
            acceptance: Ok(Acceptance {
                allowed: true,
                reject_reason: None,
                virtual_size: None,
                base_fee: None,
            }),
            actions: Mutex::new(VecDeque::new()),
            broadcast: None,
            known: Mutex::new(VecDeque::new()),
            submitted: Mutex::new(Vec::new()),
        }
    }
}

impl Node {
    pub(crate) fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    fn record(&self, event: impl Into<String>) {
        self.events.lock().unwrap().push(event.into());
    }

    /// A regtest factory whose every dependency is this node.
    pub(crate) fn factory(self: &Arc<Self>) -> WalletFactory {
        WalletFactory::assemble(
            WalletConfig::new(&Network::Regtest),
            self.clone(),
            self.clone(),
            self.clone(),
        )
    }

    /// An outbox on regtest whose every dependency is this node.
    pub(crate) fn outbox(self: &Arc<Self>) -> Outbox {
        let utxos = Arc::new(UtxoReader::new(Network::Regtest, self.clone()));
        let config = WalletConfig::new(&Network::Regtest);
        Outbox::new(&config, self.clone(), self.clone(), utxos)
    }

    /// The P2WPKH regtest address of [`Node::key`]`(byte)`.
    pub(crate) fn address(byte: u8) -> Address {
        Self::key(byte).address(Network::Regtest, AddressType::SegwitV0)
    }

    /// `satoshis` from [`Node::address`]`(byte)` to `to`, signed by its key.
    pub(crate) fn item(byte: u8, to: &Address, satoshis: u64) -> Item {
        let transfer = Transfer::new(to.clone(), Satoshi::from_atomic(satoshis));
        Item::new(Arc::new(Self::key(byte)), Self::address(byte), transfer)
    }

    /// The key of secret `[byte; 32]`.
    pub(crate) fn key(byte: u8) -> KeyPair {
        KeyPair::import(SecretBytes::new([byte; 32])).unwrap()
    }

    pub(crate) fn checkpoint() -> BlockRef {
        BlockRef {
            position: BlockPosition(1),
            height: BlockHeight(1),
            hash: BlockHash(vec![1; 32]),
            parent: None,
            timestamp: None,
        }
    }
}

impl Preflight for Node {
    fn fee_rate(
        &self,
        target_blocks: u16,
        maximum: FeeRate,
    ) -> BoxFuture<'_, Result<FeeRate, Error>> {
        assert_eq!(target_blocks, WalletConfig::FEE_TARGET_BLOCKS);
        self.record("fees");
        Box::pin(async move { self.fee.clone()?.within(maximum) })
    }

    fn accept<'a>(
        &'a self,
        _transaction: &'a SignedTransaction,
        _maximum: FeeRate,
    ) -> BoxFuture<'a, Result<Acceptance, Error>> {
        self.record("preflight");
        Box::pin(async { self.acceptance.clone() })
    }
}

impl Submitter for Node {
    fn broadcast<'a>(
        &'a self,
        transaction: SignedTransaction,
        _max_fee_rate: FeeRate,
    ) -> BoxFuture<'a, Result<TransactionId, Error>> {
        self.record("broadcast");
        let id = transaction.id;
        self.submitted.lock().unwrap().push(transaction);
        let action = self.actions.lock().unwrap().pop_front();
        Box::pin(async move {
            match action {
                Some(Action::Fail(error)) => Err(error),
                Some(Action::Gate(gate)) => {
                    gate.notified().await;
                    Ok(id)
                }
                None => self.broadcast.clone().map_or(Ok(id), Err),
            }
        })
    }

    fn known<'a>(&'a self, _id: &'a TransactionId) -> BoxFuture<'a, Result<bool, Error>> {
        self.record("known");
        let known = self.known.lock().unwrap().pop_front().unwrap_or(Ok(false));
        Box::pin(async { known })
    }
}

impl OutputStore for Node {
    /// One page at [`Node::checkpoint`]: output `i` of address `a` is
    /// `(txid [i + 1; 32], index 0)` holding the listed satoshis.
    fn list<'a>(&'a self, request: OutputRequest) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
        self.record(request.address.value.clone());
        let spent = self.spent.lock().unwrap().clone();
        let page = self.outputs.clone().map(|outputs| OutputPage {
            checkpoint: Some(Self::checkpoint()),
            outputs: outputs
                .into_iter()
                .zip(1_u8..)
                .filter(|((address, _), id)| {
                    address.as_str() == request.address.value && !spent.contains(id)
                })
                .map(|((address, value), id)| IndexedOutput {
                    id: OutputId {
                        transaction: TransactionId([id; 32]).canonical(&request.network),
                        index: 0,
                    },
                    address: request.address.clone(),
                    asset: AssetId::native(CHAIN_NAME),
                    amount: Decimal::from(value),
                    evidence: address
                        .script_pubkey(Network::Regtest)
                        .unwrap()
                        .into_bytes(),
                    created_at: BlockHeight(1),
                    coinbase: false,
                })
                .collect(),
            next: None,
        });
        Box::pin(async { page })
    }
}

impl indexing::History for Node {
    fn history<'a>(
        &'a self,
        _of: &'a dyn indexing::Holder,
        _page: indexing::Page,
    ) -> BoxFuture<'a, Result<TransactionPage, IndexError>> {
        Box::pin(async {
            Ok(TransactionPage {
                checkpoint: None,
                transactions: Vec::new(),
                pending: Vec::new(),
                next: None,
                completeness: indexing::Completeness::Incomplete,
            })
        })
    }
}

impl indexing::Checkpoint for Node {
    fn checkpoint<'a>(
        &'a self,
        _network: &'a base::Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async { Ok(Some(Self::checkpoint())) })
    }
}
