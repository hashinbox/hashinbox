//! [`Keyed`] wallets, the [`Keys`] that sign and restore them, and
//! [`Broadcasts`].

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use crypto::{Curve, SecretBytes};

use super::lock;
use crate::{
    Address, Addressed, Asset, Broadcaster, Builder, Decimal, Fee, FeeEstimator, Future, Leg,
    Network, Networked, Replace, Signer, Transaction, TransactionError, TransactionErrorKind,
    TransactionId, TransactionState, Transfer, Wallet, WalletFactory,
};

/// How the [`Keyed`] wallets of one network sign, and the
/// [`WalletFactory`] restoring them from their address text (it generates
/// `generated`). A payment's txid and bytes are the build count, with a
/// fee leg of `fee` unless zero; a replacement's are those of the one it
/// continues with `+` after, its fee one more; a cancel's `x` after, only
/// its fee, one more. Signing a replacement fails as set. Clones share
/// what was built and signed.
#[derive(Clone)]
pub struct Keys(Arc<Signing>);

struct Signing {
    network: Network,
    fee: u64,
    bump: Option<TransactionErrorKind>,
    cancel: Option<TransactionErrorKind>,
    built: Mutex<u8>,
    signed: Mutex<Vec<Transaction>>,
}

impl Keys {
    /// Keys of `network`, paying no fee, signing every replacement.
    #[must_use]
    pub fn new(network: Network) -> Self {
        Self(Arc::new(Signing {
            network,
            fee: 0,
            bump: None,
            cancel: None,
            built: Mutex::new(0),
            signed: Mutex::new(Vec::new()),
        }))
    }

    /// The same, each payment paying `fee`.
    #[must_use]
    pub fn with_fee(mut self, fee: u64) -> Self {
        self.unshared().fee = fee;
        self
    }

    /// The same, signing a bump failing as `bump`, a cancel as `cancel`
    /// (none: it signs).
    #[must_use]
    pub fn with_failures(
        mut self,
        bump: Option<TransactionErrorKind>,
        cancel: Option<TransactionErrorKind>,
    ) -> Self {
        let signing = self.unshared();
        (signing.bump, signing.cancel) = (bump, cancel);
        self
    }

    /// The wallet of the address whose text is `address`.
    #[must_use]
    pub fn keyed(&self, address: &str) -> Keyed {
        Keyed {
            address: Address::new(self.0.network.clone(), address),
            keys: self.clone(),
        }
    }

    /// How many payments were built.
    #[must_use]
    pub fn built(&self) -> u8 {
        *lock(&self.0.built)
    }

    /// Keeps `transaction` as signed, so a loaded builder state may name it.
    pub fn signed(&self, transaction: Transaction) {
        lock(&self.0.signed).push(transaction);
    }

    /// Its settings, while no clone shares them.
    fn unshared(&mut self) -> &mut Signing {
        Arc::get_mut(&mut self.0).expect("set before the keys are shared")
    }
}

impl WalletFactory for Keys {
    fn generate(&self) -> Future<'_, anyhow::Result<SecretBytes>> {
        Box::pin(async { Ok(SecretBytes::new(b"generated")) })
    }

    fn from_pk(&self, secret: SecretBytes) -> Future<'_, anyhow::Result<Arc<dyn Wallet>>> {
        let text = String::from_utf8(secret.as_bytes().to_vec());
        Box::pin(async move { Ok(Arc::new(self.keyed(&text?)) as Arc<dyn Wallet>) })
    }

    /// The key text is the address text.
    fn from_pk_text(&self, text: &str) -> Future<'_, anyhow::Result<Arc<dyn Wallet>>> {
        self.from_pk(SecretBytes::new(text.as_bytes()))
    }

    fn parse_address(&self, text: &str) -> anyhow::Result<Address> {
        Ok(Address::new(self.0.network.clone(), text))
    }

    fn transaction(&self, _asset: &Asset) -> Result<Box<dyn Builder>, TransactionError> {
        Err(TransactionError::failed("Keys build through their wallets"))
    }

    /// Up to `u64::MAX` atomic units, as most chains.
    fn carries(&self, _asset: &Asset, units: &Decimal) -> bool {
        units.to_atomic_u64(0).is_ok()
    }

    fn fees(&self) -> Arc<dyn FeeEstimator> {
        unimplemented!("Keys price no sends")
    }
}

/// A wallet whose key is its address text; it builds as its [`Keys`] sign.
#[derive(Clone)]
pub struct Keyed {
    address: Address,
    keys: Keys,
}

impl Addressed for Keyed {
    fn address(&self) -> &Address {
        &self.address
    }
}

impl Networked for Keyed {
    fn network(&self) -> &Network {
        &self.address.network
    }
}

impl Wallet for Keyed {
    fn transaction(&self, _asset: &Asset) -> Result<Box<dyn Builder>, TransactionError> {
        Ok(Box::new(Building {
            wallet: self.clone(),
            state: None,
            cancel: false,
        }))
    }

    fn private_key(&self) -> (Curve, SecretBytes) {
        let key = SecretBytes::new(self.address.value.as_bytes());
        (Curve::Secp256k1, key)
    }
}

/// A [`Keyed`] wallet's builder; its state is the txid it signed last.
struct Building {
    wallet: Keyed,
    state: Option<String>,
    cancel: bool,
}

impl Builder for Building {
    fn with_default_source(&mut self, _source: Signer) {}

    fn from_transfer(&mut self, _transfer: Transfer) -> Result<(), TransactionError> {
        Ok(())
    }

    fn with_fee(&mut self, _fee: Fee) {}

    fn sign(&mut self) -> Future<'_, Result<Transaction, TransactionError>> {
        Box::pin(async move {
            let signed = self.signed()?;
            self.state = Some(signed.id.id.clone());
            self.wallet.keys.signed(signed.clone());
            Ok(signed)
        })
    }

    fn save(&self) -> Result<serde_json::Value, TransactionError> {
        let state = self.state.clone().map(serde_json::Value::from);
        state.ok_or_else(|| TransactionError::failed("nothing signed"))
    }

    fn load(&mut self, state: &serde_json::Value) -> Result<(), TransactionError> {
        let Some(txid) = state.as_str() else {
            return Err(TransactionError::failed("not a txid"));
        };
        self.state = Some(txid.to_owned());
        Ok(())
    }

    fn replaceable(&mut self) -> Option<&mut dyn Replace> {
        Some(self)
    }
}

impl Replace for Building {
    /// Zero: a cancel; else a bump.
    fn replace(&mut self, transfer: Transfer) -> Result<(), TransactionError> {
        self.cancel = transfer.amount == Decimal::zero();
        Ok(())
    }
}

impl Building {
    /// The replacement of the transaction its state names, else a payment.
    fn signed(&self) -> Result<Transaction, TransactionError> {
        let (wallet, keys) = (&self.wallet, &*self.wallet.keys.0);
        let network = &keys.network;
        if let Some(txid) = &self.state {
            let failed = if self.cancel { keys.cancel } else { keys.bump };
            if let Some(kind) = failed {
                return Err(TransactionError::new(kind, "scripted"));
            }
            let log = lock(&keys.signed);
            let Some(existing) = log.iter().find(|signed| signed.id.id == *txid) else {
                return Err(TransactionError::failed("never signed"));
            };
            return Ok(replacement(existing, self.cancel));
        }
        let mut built = lock(&keys.built);
        *built += 1;
        let fee = Leg::Fee {
            address: wallet.address.clone(),
            asset: Asset::native(network.clone()),
            amount: keys.fee.into(),
        };
        Ok(Transaction {
            id: TransactionId::new(network.clone(), built.to_string()),
            state: TransactionState::Pending,
            order: None,
            legs: if keys.fee == 0 { Vec::new() } else { vec![fee] },
            bytes: vec![*built],
            expires_height: None,
        })
    }
}

/// `existing` at a fee one more: its txid and bytes with `+` after; its
/// cancel's with `x` after, only its fee.
fn replacement(existing: &Transaction, cancel: bool) -> Transaction {
    let txid = format!("{}{}", existing.id.id, if cancel { "x" } else { "+" });
    let legs = existing.legs.iter().cloned().filter_map(|leg| match leg {
        Leg::Fee {
            address,
            asset,
            amount,
        } => Some(Leg::Fee {
            address,
            asset,
            amount: amount + Decimal::from(1),
        }),
        _ if cancel => None,
        leg => Some(leg),
    });
    Transaction {
        id: TransactionId::new(existing.id.network.clone(), txid.clone()),
        legs: legs.collect(),
        bytes: txid.into_bytes(),
        ..existing.clone()
    }
}

/// A [`Broadcaster`] that keeps the bytes of every transaction sent and
/// answers each with the next outcome queued, accepted when none is.
#[derive(Default)]
pub struct Broadcasts {
    answers: Mutex<VecDeque<Result<(), TransactionErrorKind>>>,
    sent: Mutex<Vec<Vec<u8>>>,
}

impl Broadcasts {
    /// Broadcasts answering `answers`, in order, then accepting.
    #[must_use]
    pub fn new(answers: impl IntoIterator<Item = Result<(), TransactionErrorKind>>) -> Self {
        Self {
            answers: Mutex::new(answers.into_iter().collect()),
            sent: Mutex::default(),
        }
    }

    /// Queues `answer` after those queued.
    pub fn answer(&self, answer: Result<(), TransactionErrorKind>) {
        lock(&self.answers).push_back(answer);
    }

    /// The bytes of every transaction sent, in order.
    #[must_use]
    pub fn sent(&self) -> Vec<Vec<u8>> {
        lock(&self.sent).clone()
    }
}

impl Broadcaster for Broadcasts {
    fn broadcast<'a>(
        &'a self,
        transaction: &'a Transaction,
    ) -> Future<'a, Result<(), TransactionError>> {
        lock(&self.sent).push(transaction.bytes.clone());
        let answer = lock(&self.answers).pop_front().unwrap_or(Ok(()));
        Box::pin(async move { answer.map_err(|kind| TransactionError::new(kind, "scripted")) })
    }
}
