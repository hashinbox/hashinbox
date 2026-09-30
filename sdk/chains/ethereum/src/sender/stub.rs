//! A scripted node for send-pipeline unit tests: nonces, balances,
//! preflight contexts, broadcasts, and hash lookups, recording every call.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};

use base::BoxFuture;
use tokio::sync::Notify;

use super::Outbox;
use crate::native::{
    Address, AssetKind, Error, KeyPair, Network, SignedTransaction, TransactionId, Wei,
    rpc::{Accounts, Submitter, View},
    transaction::{BuildContext, FeeLimits, Preflight, Transfer},
};
use crate::{WalletConfig, WalletFactory};

/// The chain every fixture signs for.
pub(crate) const CHAIN_ID: u64 = 31_337;

/// What one broadcast does.
pub(crate) enum Action {
    Accept,
    /// A definite refusal.
    Reject,
    /// An unknown outcome with this message.
    Ambiguous(&'static str),
    /// Never completes.
    Pending,
    /// Accepts once the gate is notified.
    Gate(Arc<Notify>),
}

/// Every reply the double gives, and the calls it saw. Unset balances are
/// one million atomic units; unset nonces are zero.
#[derive(Default)]
pub(crate) struct Node {
    pub(crate) nonces: Mutex<BTreeMap<Address, u64>>,
    pub(crate) balances: Mutex<BTreeMap<(Address, AssetKind), Wei>>,
    pub(crate) nonce_reads: Mutex<Vec<Address>>,
    pub(crate) balance_reads: Mutex<Vec<(Address, AssetKind)>>,
    /// Every preflight call: the sender and its nonce.
    pub(crate) contexts: Mutex<Vec<(Address, u64)>>,
    /// Chain IDs the next contexts report instead of [`CHAIN_ID`].
    pub(crate) chain_ids: Mutex<VecDeque<u64>>,
    pub(crate) broadcasts: Mutex<Vec<SignedTransaction>>,
    pub(crate) actions: Mutex<VecDeque<Action>>,
    pub(crate) known_results: Mutex<VecDeque<Result<bool, Error>>>,
    pub(crate) known_ids: Mutex<Vec<TransactionId>>,
}

impl Node {
    pub(crate) fn set_nonce(&self, address: &Address, nonce: u64) {
        self.nonces.lock().unwrap().insert(address.clone(), nonce);
    }

    pub(crate) fn set_balance(&self, address: &Address, asset: &AssetKind, balance: u128) {
        self.balances
            .lock()
            .unwrap()
            .insert((address.clone(), asset.clone()), Wei::from_u128(balance));
    }

    pub(crate) fn actions(&self, actions: impl IntoIterator<Item = Action>) {
        self.actions.lock().unwrap().extend(actions);
    }

    pub(crate) fn known(&self, results: impl IntoIterator<Item = Result<bool, Error>>) {
        self.known_results.lock().unwrap().extend(results);
    }

    /// The nonce of every preflight call, in order.
    pub(crate) fn nonces_used(&self) -> Vec<u64> {
        let contexts = self.contexts.lock().unwrap();
        contexts.iter().map(|(_, nonce)| *nonce).collect()
    }

    /// An outbox whose every dependency is this node.
    pub(crate) fn outbox(self: &Arc<Self>) -> Outbox {
        Outbox::new(CHAIN_ID, self.clone(), self.clone(), self.clone())
    }

    /// A factory of ether on [`CHAIN_ID`] over this node.
    pub(crate) fn factory(self: &Arc<Self>) -> WalletFactory {
        let config = WalletConfig::new(&Network::new(CHAIN_ID, "local"));
        WalletFactory::assemble(config, self.clone(), Arc::new(self.outbox()))
    }

    fn action(&self) -> Action {
        self.actions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Action::Accept)
    }
}

/// The key of secret `[seed; 32]`, naming its own address.
pub(crate) fn signer(seed: u8) -> KeyPair {
    KeyPair::import(crypto::SecretBytes::new(vec![seed; 32])).expect("test key must be valid")
}

/// `value` wei to a fixed recipient.
pub(crate) fn native(value: u128) -> Transfer {
    Transfer {
        asset: AssetKind::Native,
        to: Address::from_bytes([0x55; 20]),
        amount: Wei::from_u128(value),
    }
}

/// `amount` of `token` to a fixed recipient.
pub(crate) fn token(token: &Address, amount: u128) -> Transfer {
    Transfer {
        asset: AssetKind::Erc20(token.clone()),
        to: Address::from_bytes([0x55; 20]),
        amount: Wei::from_u128(amount),
    }
}

impl Accounts for Node {
    fn balance<'a>(
        &'a self,
        address: Address,
        asset: &'a AssetKind,
        _at: View,
    ) -> BoxFuture<'a, Result<Wei, Error>> {
        Box::pin(async move {
            let key = (address, asset.clone());
            self.balance_reads.lock().unwrap().push(key.clone());
            let balances = self.balances.lock().unwrap();
            Ok(balances
                .get(&key)
                .cloned()
                .unwrap_or_else(|| Wei::from_u128(1_000_000)))
        })
    }

    fn nonce<'a>(&'a self, address: Address) -> BoxFuture<'a, Result<u64, Error>> {
        Box::pin(async move {
            self.nonce_reads.lock().unwrap().push(address.clone());
            Ok(self
                .nonces
                .lock()
                .unwrap()
                .get(&address)
                .copied()
                .unwrap_or(0))
        })
    }
}

impl Preflight for Node {
    fn context<'a>(
        &'a self,
        from: &'a Address,
        _transfer: &'a Transfer,
        nonce: u64,
        _limits: &'a FeeLimits,
    ) -> BoxFuture<'a, Result<BuildContext, Error>> {
        Box::pin(async move {
            self.contexts.lock().unwrap().push((from.clone(), nonce));
            let chain_id = self.chain_ids.lock().unwrap().pop_front();
            Ok(BuildContext {
                chain_id: chain_id.unwrap_or(CHAIN_ID),
                from: from.clone(),
                nonce,
                gas_limit: 1,
                max_fee_per_gas: Wei::from_u128(1),
                max_priority_fee_per_gas: Wei::from_u128(1),
            })
        })
    }
}

impl Submitter for Node {
    fn broadcast<'a>(
        &'a self,
        transaction: SignedTransaction,
    ) -> BoxFuture<'a, Result<TransactionId, Error>> {
        Box::pin(async move {
            let id = transaction.id.clone();
            self.broadcasts.lock().unwrap().push(transaction);
            match self.action() {
                Action::Accept => Ok(id),
                Action::Reject => Err(Error::new(
                    crate::native::ErrorKind::Rejected,
                    "terminal rejection",
                )),
                Action::Ambiguous(message) => Err(Error::rpc(message, true)),
                Action::Pending => std::future::pending().await,
                Action::Gate(gate) => {
                    gate.notified().await;
                    Ok(id)
                }
            }
        })
    }

    fn known<'a>(&'a self, transaction: &'a TransactionId) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async move {
            self.known_ids.lock().unwrap().push(transaction.clone());
            self.known_results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(false))
        })
    }
}

/// `transfer` from `key`'s own address, signed by `key`, within the
/// default fee limits.
pub(crate) fn item(key: &Arc<KeyPair>, transfer: Transfer) -> super::SendRequest {
    super::SendRequest::new(
        key.clone(),
        key.address().clone(),
        transfer,
        FeeLimits::default(),
    )
}
