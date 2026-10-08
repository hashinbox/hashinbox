//! A scripted node and index for adapter unit tests: fee estimates,
//! mempool preflight, submission, lookups, indexed outputs, and checkpoint,
//! recording every call.

use std::{
    collections::{BTreeSet, VecDeque},
    sync::{Arc, Mutex},
};

use base::{BlockHash, BlockHeight, BlockPosition, BlockRef, BoxFuture, SecretBytes};
use indexing::IndexError;
use wallets::{FutureResult, WalletError, WalletErrorKind};

use super::{Outbox, SendRequest, UtxoSource};
use crate::native::{
    Address, AddressType, Error, KeyPair, Network, Satoshi, SignedTransaction, TransactionId,
    block::{Outpoint, Output, Utxo},
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
        let config = WalletConfig::new(&Network::Regtest);
        Outbox::new(&config, self.clone(), self.clone(), self.clone())
    }

    /// The P2WPKH regtest address of [`Node::key`]`(byte)`.
    pub(crate) fn address(byte: u8) -> Address {
        Self::key(byte).address(Network::Regtest, AddressType::SegwitV0)
    }

    /// `satoshis` from [`Node::address`]`(byte)` to `to`, signed by its key.
    pub(crate) fn item(byte: u8, to: &Address, satoshis: u64) -> SendRequest {
        let transfer = Transfer::new(to.clone(), Satoshi::from_atomic(satoshis));
        SendRequest::new(Arc::new(Self::key(byte)), Self::address(byte), transfer)
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

impl UtxoSource for Node {
    /// Output `i` of address `a` is `(txid [i + 1; 32], index 0)` holding the
    /// listed satoshis; each address read is recorded, stopping at the first
    /// failure.
    fn utxos<'a>(&'a self, addresses: &'a [Address]) -> FutureResult<'a, Vec<Utxo>> {
        let mut utxos = Vec::new();
        for owner in addresses {
            self.record(owner.as_str());
            let outputs = match &self.outputs {
                Ok(outputs) => outputs,
                Err(error) => {
                    let failure = WalletError::new(WalletErrorKind::Unavailable, &error.message);
                    return Box::pin(async move { Err(failure) });
                }
            };
            let spent = self.spent.lock().unwrap().clone();
            let owned = outputs
                .iter()
                .zip(1_u8..)
                .filter(|((address, _), id)| address == owner && !spent.contains(id));
            utxos.extend(owned.map(|((address, value), id)| Self::utxo(address, *value, id)));
        }
        Box::pin(async move { Ok(utxos) })
    }
}

impl Node {
    /// Output number `id` of `address`, holding `value` satoshis.
    fn utxo(address: &Address, value: u64, id: u8) -> Utxo {
        let script_pubkey = address
            .script_pubkey(Network::Regtest)
            .unwrap()
            .into_bytes();
        Utxo {
            outpoint: Outpoint {
                transaction_id: TransactionId([id; 32]),
                output_index: 0,
            },
            output: Output {
                value: Satoshi::from_atomic(value),
                script_pubkey,
                address: Some(address.clone()),
            },
        }
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
