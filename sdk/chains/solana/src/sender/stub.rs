//! Doubles shared by the sender tests: keys, signed transfers, an index, and
//! registrars that hold or never acknowledge a task.

use std::sync::{Arc, Mutex};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use json_rpc::{Batch, CallResult, RawJson, Request};
use serde_json::{Value, json};

use base::{BlockHash, BlockHeight, BlockPosition, BlockRef, BoxFuture, Networked};
use indexing::{Checkpoint, IndexError, Network};
use solana_hash::Hash;

use wallets::{RegistrationError, RegistrationFuture, SubmissionRegistrar, SubmissionTask};

use super::{Landing, Outbox, Presence, recovery::Settler, reservation::Reservations};
use crate::native::{
    Address, KeyPair, Lamport, Memo, Seed, SignedTransaction,
    rpc::{
        Client,
        script::{Script, Scripted},
    },
    transaction::{Lifetime, Transfer, UnsignedTransaction},
};
use crate::{WalletConfig, WalletFactory};

pub(crate) fn network() -> Network {
    Script::network().canonical()
}

/// The key whose seed is 32 bytes of `byte`.
pub(crate) fn key(byte: u8) -> Arc<KeyPair> {
    let seed = hex::encode([byte; 32])
        .parse::<Seed>()
        .expect("fixture seed");
    Arc::new(KeyPair::import(seed.into()).expect("fixture key"))
}

/// The blockhash lifetime every fixture uses: hash `[9; 32]`, valid through 44.
pub(crate) fn lifetime() -> Lifetime {
    Lifetime::new(Hash::new_from_array([9; 32]), 44)
}

/// Three lamports from `key` to `[8; 32]` with memo `memo`, signed at `min_slot`.
pub(crate) fn signed(key: &KeyPair, memo: u8, min_slot: u64) -> SignedTransaction {
    let transfer = Transfer {
        to: Address::from_bytes([8; 32]),
        amount: Lamport::from_atomic(3),
    };
    let message = UnsignedTransaction::new(
        key.address(),
        &transfer,
        Memo::from_bytes([memo; Memo::LENGTH]),
        &lifetime(),
    );
    futures_executor::block_on(message.sign(key.address(), lifetime(), min_slot, key))
        .expect("fixture signature")
}

/// An index checkpoint at produced height `height`.
pub(crate) fn checkpoint(height: u64) -> BlockRef {
    BlockRef {
        position: BlockPosition(height + 100),
        height: BlockHeight(height),
        hash: BlockHash(vec![u8::try_from(height % 256).unwrap_or(0); 32]),
        parent: None,
        timestamp: None,
    }
}

/// An index that never holds the transaction, answering at `page_checkpoint`.
#[derive(Default)]
pub(crate) struct Index {
    pub(crate) checkpoint: Option<BlockRef>,
    pub(crate) page_checkpoint: Option<BlockRef>,
}

impl Checkpoint for Index {
    fn checkpoint<'a>(
        &'a self,
        _network: &'a Network,
    ) -> BoxFuture<'a, Result<Option<BlockRef>, IndexError>> {
        Box::pin(async move { Ok(self.checkpoint.clone()) })
    }
}

impl Presence for Index {
    fn landed<'a>(
        &'a self,
        _transaction: &'a base::TransactionRef,
    ) -> BoxFuture<'a, Result<Landing, IndexError>> {
        Box::pin(async move {
            Ok(Landing {
                checkpoint: self.page_checkpoint.clone(),
                present: false,
            })
        })
    }
}

/// Keeps every task for the test to run, after acknowledging it, refusing
/// it, or never answering at all.
pub(crate) struct Registrar {
    answer: Answer,
    tasks: Arc<Mutex<Vec<SubmissionTask>>>,
}

enum Answer {
    Acknowledge,
    Refuse(RegistrationError),
    /// Runs the task without ever acknowledging it.
    Withhold,
}

impl Registrar {
    fn new(answer: Answer) -> Arc<Self> {
        Arc::new(Self {
            answer,
            tasks: Arc::default(),
        })
    }

    pub(crate) fn accepting() -> Arc<Self> {
        Self::new(Answer::Acknowledge)
    }

    pub(crate) fn closed() -> Arc<Self> {
        Self::new(Answer::Refuse(RegistrationError::Closed))
    }

    pub(crate) fn withholding() -> Arc<Self> {
        Self::new(Answer::Withhold)
    }

    /// The oldest task still held.
    pub(crate) fn task(&self) -> SubmissionTask {
        self.tasks.lock().unwrap().remove(0)
    }
}

impl SubmissionRegistrar for Registrar {
    fn register<'a>(&'a self, task: SubmissionTask) -> RegistrationFuture<'a> {
        Box::pin(async move {
            match self.answer {
                Answer::Acknowledge => {
                    self.tasks.lock().unwrap().push(task);
                    Ok(())
                }
                Answer::Refuse(error) => Err(error),
                Answer::Withhold => {
                    task.run().await;
                    panic!("an unacknowledged registration must not activate its task")
                }
            }
        })
    }
}

/// Records every transaction handed to recovery and settles nothing.
#[derive(Default)]
pub(crate) struct Resolution(pub(crate) Mutex<Vec<SignedTransaction>>);

impl Settler for Resolution {
    fn settle(&self, transaction: SignedTransaction) -> BoxFuture<'_, ()> {
        Box::pin(async move { self.0.lock().unwrap().push(transaction) })
    }
}

/// A factory for `localnet` over `client`, sending through `registrar`.
/// Recovery records and never settles.
pub(crate) fn factory<C: json_rpc::Client + 'static>(
    client: Client<C>,
    registrar: Arc<dyn SubmissionRegistrar>,
) -> WalletFactory {
    let outbox = outbox(client.clone(), registrar, Arc::new(Resolution::default()));
    WalletFactory::assemble(
        WalletConfig::new(&Script::network()),
        Arc::new(client),
        Arc::new(outbox),
    )
}

/// An outbox over `client` for every node read, with its own reservations.
pub(crate) fn outbox<C: json_rpc::Client + 'static>(
    client: Client<C>,
    registrar: Arc<dyn SubmissionRegistrar>,
    recovery: Arc<dyn Settler>,
) -> Outbox {
    let rpc = Arc::new(client);
    Outbox::new(
        rpc.clone(),
        rpc.clone(),
        rpc,
        registrar,
        recovery,
        Reservations::default(),
    )
}

/// A cooperative node: every account is an empty wallet holding ten SOL,
/// fees are 5000 lamports, simulation passes, and a submission either lands
/// (the node returns its signature) or, with `lost` set, is lost (the send
/// fails and no status ever appears).
#[derive(Clone, Default)]
pub(crate) struct Node {
    pub(crate) lost: bool,
    pub(crate) methods: Arc<Mutex<Vec<String>>>,
}

impl Node {
    // linter:disable rust/string-backed-finite-state -- these are JSON-RPC method names on the
    // wire, dispatched exactly as the transport dispatches them; the closed vocabulary belongs to
    // Solana's RPC, not to this crate.
    fn reply(&self, method: &str, params: &Value) -> Result<Value, json_rpc::Error> {
        let slot = json!({"slot": 10});
        let reply = match method {
            "getHealth" => json!("ok"),
            "getSlot" | "getBlockHeight" => json!(10),
            "getMultipleAccounts" => {
                let count = params[0].as_array().map_or(0, Vec::len);
                let owner = Address::from_bytes([0; 32]).to_string();
                let account = json!({
                    "lamports": 10_000_000_000_u64, "owner": owner,
                    "executable": false, "data": ["", "base64"], "space": 0,
                });
                json!({"context": slot, "value": vec![account; count]})
            }
            "getLatestBlockhash" => json!({"context": slot, "value": {
                "blockhash": Hash::new_from_array([9; 32]).to_string(), "lastValidBlockHeight": 44,
            }}),
            "getFeeForMessage" => json!({"context": slot, "value": 5000}),
            "simulateTransaction" => json!({"context": slot, "value": {"err": null}}),
            "sendTransaction" if self.lost => {
                return Err(json_rpc::Error {
                    kind: json_rpc::ErrorKind::Timeout,
                    message: "lost".to_owned(),
                });
            }
            "sendTransaction" => {
                let bytes = STANDARD
                    .decode(params[0].as_str().unwrap_or_default())
                    .unwrap_or_default();
                let sent: solana_transaction::Transaction =
                    bincode::deserialize(&bytes).expect("a signed transaction");
                json!(sent.signatures[0].to_string())
            }
            "getSignatureStatuses" => json!({"context": slot, "value": [null]}),
            other => panic!("unexpected {other}"),
        };
        Ok(reply)
    }
}

impl json_rpc::Client for Node {
    fn request<'a>(
        &'a self,
        request: Request,
    ) -> BoxFuture<'a, Result<CallResult, json_rpc::Error>> {
        let call = request.into_call();
        self.methods.lock().unwrap().push(call.method.clone());
        let reply = self
            .reply(&call.method, &call.params)
            .map(|value| Ok(RawJson::from_serializable(&value).expect("reply")));
        Box::pin(async move { reply })
    }

    fn batch<'a>(
        &'a self,
        _batch: Batch,
    ) -> BoxFuture<'a, Result<Vec<CallResult>, json_rpc::Error>> {
        panic!("Solana never batches")
    }
}
