//! The chain-free transaction path: hold a wallet, build, inspect, sign,
//! read the fee, broadcast — through `Arc<dyn Wallet>` only.
//!
//! This file names no chain crate, and by `linter.toml`'s layer rule it
//! cannot: `wallets` may not depend on one. That is the point. Every step
//! below was reachable before only by downcasting `Arc<dyn Wallet>` to a
//! concrete chain wallet (`sdk/chains/solana/src/builder.rs` did exactly
//! that until `TransactionFactory` joined `Wallet`'s supertraits).
//!
//! The fixture's outbox reproduces the reservation rule all three chains
//! implement: `prepare` signs and *holds* the transaction while releasing
//! the scarce thing it spends, and `broadcast` takes the hold back exactly
//! once.

use std::{
    fmt,
    sync::{Arc, Mutex},
};

use base::{
    Address, Broadcaster, ChainName, Decimal, Draft, Network, SignFuture, SignRequest,
    SignedTransaction, Signer, Submission, TransactionBuilder, TransactionError,
    TransactionErrorKind, TransactionFuture, TransactionId, TransactionSnapshot,
};
use futures_executor::block_on;
use wallets::{
    AddressParser, Author, Authorship, FactoryProfile, FutureResult, Generation, Holder,
    SecretBytes, SendFuture, Sender, SingleSender, TransactionFactory, Transfer, Wallet,
    WalletFactory, Wallets,
};

/// The fee every prepared fixture transaction pays, in atomic units.
const FEE: u64 = 125;

const SIGNED_KIND: &str = "example.signed";
const SNAPSHOT_KIND: &str = "example.transfer";

fn network() -> Network {
    Network::new(ChainName::new("example"), "mainnet")
}

/// The caller's own asset key; `Wallets<I, A>` is generic over it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Tag(&'static str);

impl fmt::Display for Tag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

const COIN_TAG: Tag = Tag("coin");

#[derive(Default)]
struct Outbox {
    events: Mutex<Vec<String>>,
    held: Mutex<Option<TransactionId>>,
}

impl Outbox {
    fn record(&self, event: &str) {
        self.events.lock().expect("events").push(event.to_owned());
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().expect("events").clone()
    }

    /// Signs, then holds the result while releasing what it spends, exactly
    /// as every chain's `Outbox::sign` does.
    fn sign(&self, id: TransactionId) -> SignedTransaction {
        self.record("reserve");
        self.record("sign");
        *self.held.lock().expect("held") = Some(id.clone());
        self.record("release");
        SignedTransaction::new(
            SIGNED_KIND,
            id,
            base::transaction::Envelope::new([1, 2, 3]),
            Decimal::from(FEE),
        )
    }

    /// Takes the hold back, once. Anything else is a definite failure.
    fn broadcast(&self, id: &TransactionId) -> Result<Submission, TransactionError> {
        let mut held = self.held.lock().expect("held");
        if held.as_ref() != Some(id) {
            return Err(TransactionError::new(
                TransactionErrorKind::InvalidTransaction,
                "transaction was not prepared by this wallet or was broadcast",
            ));
        }
        *held = None;
        self.record("submit");
        Ok(Submission { id: id.clone() })
    }
}

struct Factory(Arc<Outbox>);

impl WalletFactory for Factory {
    fn create<'a>(&'a self, secret: SecretBytes) -> FutureResult<'a, Arc<dyn Wallet>> {
        let address = Address::new(network(), format!("addr-{:02x}", secret.as_bytes()[0]));
        let outbox = Arc::clone(&self.0);
        let asset = base::AssetId::native(ChainName::new("example"));
        Box::pin(async move {
            Ok(Arc::new(Fixture {
                address,
                asset,
                outbox,
            }) as Arc<dyn Wallet>)
        })
    }

    fn generate(&self) -> FutureResult<'_, Generation> {
        Generation::of(self, Ok(SecretBytes::new([0x42])))
    }

    fn validate(&self, _wallet: &dyn Wallet) -> Result<(), wallets::WalletError> {
        Ok(())
    }
}

impl AddressParser for Factory {
    fn parse_address(&self, text: &str) -> Result<Address, wallets::WalletError> {
        if !text.starts_with("addr-") {
            return Err(wallets::WalletError::new(
                wallets::WalletErrorKind::InvalidAddress,
                "fixture address must start with addr-",
            ));
        }
        Ok(Address::new(network(), text.to_owned()))
    }
}

impl FactoryProfile for Factory {
    fn network(&self) -> &Network {
        static NETWORK: std::sync::OnceLock<Network> = std::sync::OnceLock::new();
        NETWORK.get_or_init(network)
    }

    fn asset(&self) -> base::AssetId {
        base::AssetId::native(ChainName::new("example"))
    }

    /// The example asset is one this system spends, so its factory declares
    /// an authorship; an asset it only received would declare `None` and
    /// every test below would stop at `Wallets::author`.
    fn authorship(&self) -> Option<Arc<dyn Authorship>> {
        Some(Arc::new(Batch))
    }
}

struct Batch;

impl Sender for Batch {
    fn send<'a>(&'a self, _transfers: Vec<Transfer>) -> SendFuture<'a> {
        Box::pin(async { Ok(vec![TransactionId::new("batch")]) })
    }
}

impl Authorship for Batch {
    fn author(&self, wallet: &Arc<dyn Wallet>) -> Result<Arc<dyn Author>, wallets::WalletError> {
        let wallet = (wallet.as_ref() as &dyn std::any::Any)
            .downcast_ref::<Fixture>()
            .ok_or_else(|| {
                wallets::WalletError::new(
                    wallets::WalletErrorKind::Unsupported,
                    "wallet was not made by this factory",
                )
            })?;
        Ok(Arc::new(wallet.clone()))
    }
}

/// A wallet with an address, an asset, an outbox, and a builder.
#[derive(Clone)]
struct Fixture {
    address: Address,
    asset: base::AssetId,
    outbox: Arc<Outbox>,
}

impl Holder for Fixture {
    fn address(&self) -> &Address {
        &self.address
    }

    fn asset(&self) -> &base::AssetId {
        &self.asset
    }
}

impl Signer for Fixture {
    fn sign<'a>(&'a self, _request: SignRequest) -> SignFuture<'a> {
        Box::pin(async { unreachable!("the fixture outbox signs") })
    }
}

impl SingleSender for Fixture {
    fn send<'a>(
        &'a self,
        _to: Address,
        _amount: wallets::Amount,
    ) -> FutureResult<'a, TransactionId> {
        Box::pin(async { Ok(TransactionId::new("single")) })
    }
}

impl TransactionFactory for Fixture {
    fn transaction(&self) -> Box<dyn TransactionBuilder> {
        Box::new(Builder {
            source: self.address.clone(),
            transfers: Vec::new(),
            outbox: Arc::clone(&self.outbox),
        })
    }

    fn resume(
        &self,
        snapshot: &TransactionSnapshot,
    ) -> Result<Box<dyn TransactionBuilder>, TransactionError> {
        if snapshot.kind() != SNAPSHOT_KIND || snapshot.value()["source"] != self.address.value {
            return Err(TransactionError::new(
                TransactionErrorKind::InvalidSnapshot,
                "snapshot belongs to another wallet",
            ));
        }
        let amount = snapshot.value()["amount"]
            .as_str()
            .and_then(|text| text.parse::<Decimal>().ok())
            .ok_or_else(|| {
                TransactionError::new(TransactionErrorKind::InvalidSnapshot, "invalid amount")
            })?;
        let to = Address::new(
            network(),
            snapshot.value()["destination"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        );
        Ok(Box::new(Builder {
            source: self.address.clone(),
            transfers: vec![base::Transfer { to, amount }],
            outbox: Arc::clone(&self.outbox),
        }))
    }

    fn broadcaster(&self) -> &dyn Broadcaster {
        self
    }
}

impl Broadcaster for Fixture {
    fn broadcast<'a>(
        &'a self,
        transaction: &'a SignedTransaction,
    ) -> TransactionFuture<'a, Result<Submission, TransactionError>> {
        Box::pin(async move {
            if transaction.kind() != SIGNED_KIND {
                return Err(TransactionError::new(
                    TransactionErrorKind::InvalidTransaction,
                    "not a fixture envelope",
                ));
            }
            self.outbox.broadcast(transaction.id())
        })
    }
}

struct Builder {
    source: Address,
    transfers: Vec<base::Transfer>,
    outbox: Arc<Outbox>,
}

impl TransactionBuilder for Builder {
    fn transfer(&mut self, to: Address, amount: Decimal) -> Result<(), TransactionError> {
        self.transfers.push(base::Transfer { to, amount });
        Ok(())
    }

    fn draft(&self) -> Result<Draft, TransactionError> {
        let transfer = self.transfers.first().ok_or_else(|| {
            TransactionError::new(
                TransactionErrorKind::InvalidTransaction,
                "transaction has no recipients",
            )
        })?;
        Ok(Draft {
            source: self.source.clone(),
            transfers: self.transfers.clone(),
            asset: base::AssetId::native(ChainName::new("example")),
            snapshot: TransactionSnapshot::new(
                SNAPSHOT_KIND,
                serde_json::json!({
                    "source": self.source.value,
                    "destination": transfer.to.value,
                    "amount": transfer.amount.to_string(),
                }),
            ),
        })
    }

    fn prepare<'a>(
        &'a mut self,
    ) -> TransactionFuture<'a, Result<SignedTransaction, TransactionError>> {
        Box::pin(async move {
            self.draft()?;
            Ok(self.outbox.sign(TransactionId::new("prepared-id")))
        })
    }
}

fn collection(outbox: &Arc<Outbox>) -> Wallets<Tag> {
    let mut wallets = Wallets::new();
    wallets
        .add(COIN_TAG, Factory(Arc::clone(outbox)))
        .expect("registration");
    wallets
}

/// Section 1.8 of `docs/design/030 §8 (ex-026)`, end to end, with no chain named: the
/// map hands back a wallet, the wallet hands back a builder, the draft says
/// what will be paid before any node is asked, the signed transaction says
/// what it costs before anything leaves the process, and only `broadcast`
/// has an external effect.
#[test]
fn a_held_wallet_builds_inspects_signs_and_broadcasts_without_naming_a_chain() {
    let outbox = Arc::new(Outbox::default());
    let wallets = collection(&outbox);

    // The map makes the wallet and hands it over; the caller keeps it.
    let held: Arc<dyn Wallet> = block_on(wallets.create(&COIN_TAG)).expect("create");
    // The capability is the map's to give, and only for an asset it spends.
    let wallet = wallets
        .author(&COIN_TAG, &held)
        .expect("an asset it spends");
    let mut builder = wallet.transaction();
    let destination = wallets.parse(&COIN_TAG, "addr-99").expect("destination");
    builder
        .transfer(destination.clone(), "1.25".parse().expect("amount"))
        .expect("one transfer");

    let draft = builder.draft().expect("draft");
    assert_eq!(draft.source, *wallet.address());
    assert_eq!(
        draft.asset,
        base::AssetId::native(ChainName::new("example"))
    );
    assert_eq!(
        draft
            .transfers
            .iter()
            .map(|transfer| (transfer.to.value.as_str(), transfer.amount.to_string()))
            .collect::<Vec<_>>(),
        [("addr-99", "1.25".to_owned())]
    );
    assert!(
        outbox.events().is_empty(),
        "a draft asks no node and signs nothing"
    );

    let signed = block_on(builder.prepare()).expect("prepared");
    assert_eq!(*signed.fee(), Decimal::from(FEE));
    assert_eq!(signed.id().as_str(), "prepared-id");
    assert_eq!(outbox.events(), ["reserve", "sign", "release"]);

    let submission = block_on(wallet.broadcaster().broadcast(&signed)).expect("broadcast");
    assert_eq!(&submission.id, signed.id());
    assert_eq!(outbox.events(), ["reserve", "sign", "release", "submit"]);
}

/// The reservation rule the builder path must not weaken: `prepare` holds
/// the transaction for exactly one `broadcast`, and a transaction this
/// wallet never prepared — or already broadcast — fails *definitely*, so no
/// caller can mistake it for a retryable outcome.
#[test]
fn preparation_holds_one_transaction_for_exactly_one_broadcast() {
    let outbox = Arc::new(Outbox::default());
    let wallets = collection(&outbox);
    let held = block_on(wallets.create(&COIN_TAG)).expect("create");
    let wallet = wallets
        .author(&COIN_TAG, &held)
        .expect("an asset it spends");

    let mut builder = wallet.transaction();
    builder
        .transfer(
            Address::new(network(), "addr-99".to_owned()),
            "1".parse().expect("amount"),
        )
        .expect("one transfer");
    let signed = block_on(builder.prepare()).expect("prepared");

    block_on(wallet.broadcaster().broadcast(&signed)).expect("the first broadcast takes the hold");
    let error =
        block_on(wallet.broadcaster().broadcast(&signed)).expect_err("the hold is taken only once");
    assert_eq!(error.kind, TransactionErrorKind::InvalidTransaction);
    assert_eq!(error.ambiguous_transaction_id, None);
    assert_eq!(outbox.events(), ["reserve", "sign", "release", "submit"]);
}

/// A draft is durable: persist its snapshot, rebuild the builder from it,
/// and the same transaction is prepared. A snapshot of another wallet is
/// refused.
#[test]
fn a_draft_snapshot_resumes_its_own_wallet_and_no_other() {
    let outbox = Arc::new(Outbox::default());
    let wallets = collection(&outbox);
    let held = block_on(wallets.create(&COIN_TAG)).expect("alice");
    let alice = wallets
        .author(&COIN_TAG, &held)
        .expect("an asset it spends");
    let bob = wallets
        .author(
            &COIN_TAG,
            &block_on(wallets.from(&COIN_TAG, SecretBytes::new([0x07]))).expect("bob"),
        )
        .expect("an asset it spends");
    wallets
        .accepts(&COIN_TAG, held.as_ref())
        .expect("this factory made alice");

    let mut builder = alice.transaction();
    builder
        .transfer(
            Address::new(network(), "addr-99".to_owned()),
            "2.5".parse().expect("amount"),
        )
        .expect("one transfer");
    let snapshot = builder.draft().expect("draft").snapshot;

    let resumed = alice
        .resume(&snapshot)
        .expect("alice resumes her own draft");
    assert_eq!(resumed.draft().expect("draft").snapshot, snapshot);
    assert_eq!(
        bob.resume(&snapshot)
            .err()
            .expect("a draft is bound to its wallet")
            .kind,
        TransactionErrorKind::InvalidSnapshot
    );
    assert!(
        outbox.events().is_empty(),
        "resuming a draft asks no node and signs nothing"
    );
}
