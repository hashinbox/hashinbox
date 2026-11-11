//! Wallets, building and signing, and broadcast, over scripted nodes.

use std::sync::Arc;

use ::rpc::{
    BoxFuture, CallError, Cluster, Policy,
    testing::{Reply, Script},
};
use base::{
    Asset, Broadcaster as _, BuilderExt as _, Decimal, Leg, Signer, TransactionErrorKind,
    TransactionState, Transfer, Wallet, WalletError, WalletErrorKind, WalletFactory as _,
};
use chain_solana::{Client, Config};
use crypto::SecretBytes;
use futures_executor::block_on;
use serde_json::json;
use solana_keypair::{Keypair, Signer as _};
use solana_transaction::Transaction;

const MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const SEED: [u8; 32] = [7; 32];

fn base58(byte: u8, length: usize) -> String {
    bs58::encode(vec![byte; length]).into_string()
}

fn config() -> Config {
    let genesis_hash = base58(9, 32).parse().unwrap();
    let network = chain_solana::Network::new(genesis_hash, "localnet");
    let mut config = Config::new(network);
    config.tokens = vec![chain_solana::Token {
        mint: MINT.parse().unwrap(),
        decimals: 6,
        ticker: None,
        display_decimals: None,
    }];
    config
}

fn network() -> base::Network {
    base::Network::from(&config().native())
}

fn client(transport: impl ::rpc::Client + 'static) -> Client {
    Client::new(Arc::new(transport), config().native())
}

fn wallet(script: &Script) -> Arc<dyn Wallet> {
    let factory = config().factory(&client(script.clone()), Arc::new(base::NoHoldings));
    block_on(factory.from_pk(SecretBytes::new(SEED))).unwrap()
}

/// An on-curve recipient: the address of another key.
fn recipient() -> base::Address {
    let key = Keypair::new_from_array([8; 32]);
    base::Address::new(network(), key.pubkey().to_string())
}

fn signing_replies() -> [Reply; 2] {
    [
        Reply::ok(
            "getLatestBlockhash",
            json!({ "context": { "slot": 1 }, "value": { "blockhash": base58(3, 32), "lastValidBlockHeight": 50 } }),
        ),
        Reply::ok(
            "getFeeForMessage",
            json!({ "context": { "slot": 1 }, "value": 5000 }),
        ),
    ]
}

/// `sources` as spends to new addresses.
fn spends(sources: &[(base::Address, Asset)]) -> base::Spends {
    sources.iter().cloned().collect()
}

/// `sources` as spends to `to`.
fn spends_to(sources: &[(base::Address, Asset)], to: &base::Address) -> base::Spends {
    sources
        .iter()
        .cloned()
        .map(|(from, asset)| (from, asset, to.clone()))
        .collect()
}

#[test]
fn a_wallet_keeps_its_ed25519_key_and_parses_canonical_addresses() {
    let script = Script::default();
    let wallet = wallet(&script);
    let reference = Keypair::new_from_array(SEED);
    assert_eq!(wallet.address().value, reference.pubkey().to_string());
    assert_eq!(wallet.private_key().1.as_bytes(), SEED);

    let factory = config().factory(&client(script), Arc::new(base::NoHoldings));
    let parsed = factory.parse_address(&wallet.address().value).unwrap();
    assert_eq!(&parsed, wallet.address());
    let error = factory.parse_address("0x00").unwrap_err();
    assert_eq!(
        error.downcast_ref::<WalletError>().map(|error| error.kind),
        Some(WalletErrorKind::InvalidAddress)
    );
    let short = block_on(factory.from_pk(SecretBytes::new([1; 31])))
        .err()
        .unwrap();
    assert_eq!(
        short.downcast_ref::<WalletError>().map(|error| error.kind),
        Some(WalletErrorKind::InvalidSecret)
    );
}

#[test]
fn sol_transfers_sign_into_one_verifying_transaction_with_its_legs() {
    let script = Script::new(signing_replies());
    let wallet = wallet(&script);
    let sol = Asset::native(network());
    let mut builder = wallet.transaction(&sol).unwrap();
    builder.send((recipient(), Decimal::from(100))).unwrap();
    builder.send((recipient(), Decimal::from(200))).unwrap();

    let signed = block_on(builder.sign()).unwrap();

    let [transaction] = &signed[..] else {
        panic!("both transfers fit one transaction");
    };
    let decoded: Transaction = bincode::deserialize(&transaction.bytes).unwrap();
    decoded.verify().unwrap();
    assert_eq!(
        decoded.message.instructions.len(),
        3,
        "two transfers and a memo"
    );
    assert_eq!(decoded.message.recent_blockhash.to_bytes(), [3; 32]);
    let reference = Keypair::new_from_array(SEED).sign_message(&decoded.message.serialize());
    assert_eq!(
        transaction.id.id,
        reference.to_string(),
        "crypto::Ed25519 signs as Solana does"
    );
    assert_eq!(transaction.state, TransactionState::Pending);
    let from = wallet.address().clone();
    assert_eq!(
        transaction.legs,
        vec![
            Leg::Fee {
                address: from.clone(),
                asset: sol.clone(),
                amount: Decimal::from(5000)
            },
            Leg::Spent {
                address: from.clone(),
                asset: sol.clone(),
                amount: Decimal::from(100),
                coin: None
            },
            Leg::Received {
                address: recipient(),
                asset: sol.clone(),
                amount: Decimal::from(100),
                coin: None
            },
            Leg::Spent {
                address: from,
                asset: sol.clone(),
                amount: Decimal::from(200),
                coin: None
            },
            Leg::Received {
                address: recipient(),
                asset: sol,
                amount: Decimal::from(200),
                coin: None
            },
        ]
    );
    script.exhausted();
}

#[test]
fn a_token_transfer_creates_the_recipient_account_and_transfers_checked() {
    let script = Script::new(signing_replies());
    let wallet = wallet(&script);
    let usdc = Asset {
        network: network(),
        id: MINT.to_owned(),
    };
    let mut builder = wallet.transaction(&usdc).unwrap();
    builder.send((recipient(), Decimal::from(20))).unwrap();

    let signed = block_on(builder.sign()).unwrap();

    let decoded: Transaction = bincode::deserialize(&signed[0].bytes).unwrap();
    let programs: Vec<String> = decoded
        .message
        .instructions
        .iter()
        .map(|instruction| {
            decoded.message.account_keys[usize::from(instruction.program_id_index)].to_string()
        })
        .collect();
    assert_eq!(
        programs[..2],
        [
            "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        ]
    );
    let mut data = vec![12];
    data.extend_from_slice(&20_u64.to_le_bytes());
    data.push(6);
    assert_eq!(decoded.message.instructions[1].data, data);
    let accounts = |index: usize| -> Vec<String> {
        decoded.message.instructions[index]
            .accounts
            .iter()
            .map(|account| decoded.message.account_keys[usize::from(*account)].to_string())
            .collect()
    };
    let from = wallet.address().value.clone();
    let (sender, receiver) = (
        associated(&from).to_string(),
        associated(&recipient().value).to_string(),
    );
    assert_eq!(
        accounts(0)[..4],
        [
            from.clone(),
            receiver.clone(),
            recipient().value,
            MINT.to_owned()
        ],
        "create the recipient's token account, paid by the sender"
    );
    assert_eq!(accounts(1), [sender, MINT.to_owned(), receiver, from]);
    assert_eq!(
        signed[0].legs[1..],
        [
            Leg::Spent {
                address: wallet.address().clone(),
                asset: usdc.clone(),
                amount: Decimal::from(20),
                coin: None
            },
            Leg::Received {
                address: recipient(),
                asset: usdc,
                amount: Decimal::from(20),
                coin: None
            },
        ]
    );
}

#[test]
fn builders_refuse_what_they_cannot_send() {
    let script = Script::default();
    let wallet = wallet(&script);
    let other = Asset {
        network: network(),
        id: base58(8, 32),
    };
    assert_eq!(
        wallet.transaction(&other).err().map(|error| error.kind),
        Some(TransactionErrorKind::Unsupported)
    );
    let mut builder = wallet.transaction(&Asset::native(network())).unwrap();
    let refused = |result: Result<(), base::TransactionError>| result.unwrap_err().kind;
    assert_eq!(
        refused(builder.send((recipient(), Decimal::zero()))),
        TransactionErrorKind::Failed
    );
    assert_eq!(
        refused(builder.send((recipient(), "0.5".parse().unwrap()))),
        TransactionErrorKind::Failed
    );
    assert_eq!(
        refused(builder.send((wallet.address().clone(), Decimal::from(1)))),
        TransactionErrorKind::Failed
    );
    let off_curve = base::Address::new(network(), associated(&wallet.address().value).to_string());
    assert_eq!(
        refused(builder.send((off_curve, Decimal::from(1)))),
        TransactionErrorKind::Failed
    );
    assert_eq!(
        block_on(builder.sign()).unwrap_err().kind,
        TransactionErrorKind::Failed
    );
}

fn signed(script: &Script) -> base::Transaction {
    let wallet = wallet(script);
    let mut builder = wallet.transaction(&Asset::native(network())).unwrap();
    builder.send((recipient(), Decimal::from(100))).unwrap();
    block_on(builder.sign()).unwrap().0.remove(0)
}

fn probe(_: &Script) -> BoxFuture<'_, Result<u64, CallError>> {
    Box::pin(async { Ok(0) })
}

#[test]
fn broadcast_sends_the_exact_bytes_to_every_node() {
    let transaction = signed(&Script::new(signing_replies()));
    let nodes: Vec<Script> = (0..2)
        .map(|_| {
            Script::new([Reply::ok("sendTransaction", json!(transaction.id.id))])
                .once_only("sendTransaction")
        })
        .collect();
    let cluster = Cluster::new(nodes.clone(), probe, Policy::default()).unwrap();
    let broadcaster = client(cluster);

    block_on(broadcaster.broadcast(&transaction)).unwrap();

    for node in &nodes {
        let requests = node.requests();
        let encoded = requests[0].1[0].as_str().unwrap();
        assert_eq!(base64_decode(encoded), transaction.bytes);
        node.exhausted();
    }
}

#[test]
fn broadcast_classifies_refusals() {
    let transaction = signed(&Script::new(signing_replies()));
    let outcome = |reply: Reply| {
        let broadcaster = client(Script::new([reply]));
        block_on(broadcaster.broadcast(&transaction)).map_err(|error| error.kind)
    };
    let refused = |message: &str| Reply::failure("sendTransaction", -32_002, message);
    assert_eq!(
        outcome(refused(
            "Transaction simulation failed: Blockhash not found"
        )),
        Err(TransactionErrorKind::Expired)
    );
    assert_eq!(
        outcome(refused(
            "Transaction simulation failed: This transaction has already been processed"
        )),
        Ok(())
    );
    assert_eq!(
        outcome(refused(
            "Transaction simulation failed: Error processing Instruction 0"
        )),
        Err(TransactionErrorKind::Rejected)
    );
    assert_eq!(
        outcome(refused(
            "Transaction simulation failed: insufficient funds for rent"
        )),
        Err(TransactionErrorKind::Rejected)
    );
}

fn base64_decode(text: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .unwrap()
}

/// The associated token account of `owner` for [`MINT`].
fn associated(owner: &str) -> solana_address::Address {
    let token =
        solana_address::Address::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
    let program =
        solana_address::Address::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
    let owner: solana_address::Address = owner.parse().unwrap();
    let mint: solana_address::Address = MINT.parse().unwrap();
    solana_address::Address::try_find_program_address(
        &[owner.as_array(), token.as_array(), mint.as_array()],
        &program,
    )
    .unwrap()
    .0
}

#[test]
fn two_sources_sign_one_transaction_each() {
    let script = Script::new([]);
    let factory = config().factory(&client(script.clone()), Arc::new(base::NoHoldings));
    let one = block_on(factory.from_pk(SecretBytes::new(SEED))).unwrap();
    let two = block_on(factory.from_pk(SecretBytes::new([9; 32]))).unwrap();
    let mut builder = factory.transaction(&Asset::native(network())).unwrap();
    builder
        .send(Transfer::new(recipient(), Decimal::from(100)).from(&one))
        .unwrap();
    builder
        .send(Transfer::new(recipient(), Decimal::from(200)).from(&two))
        .unwrap();
    script.push(signing_replies());
    let signed = block_on(builder.sign()).unwrap();
    script.exhausted();
    assert_eq!(signed.len(), 2, "one per sender");
    for (transaction, sender) in signed.iter().zip([&one, &two]) {
        let decoded: Transaction = bincode::deserialize(&transaction.bytes).unwrap();
        decoded.verify().unwrap();
        assert_eq!(
            decoded.message.account_keys[0].to_string(),
            sender.address().value,
            "paid and signed by its sender"
        );
    }
}

#[test]
fn a_factory_builder_needs_a_source_whose_key_signs_for_it() {
    let script = Script::new([]);
    let factory = config().factory(&client(script.clone()), Arc::new(base::NoHoldings));
    let mut builder = factory.transaction(&Asset::native(network())).unwrap();
    builder.send((recipient(), Decimal::from(100))).unwrap();
    assert_eq!(
        block_on(builder.sign()).unwrap_err().kind,
        TransactionErrorKind::Failed
    );

    let wallet = wallet(&script);
    let wrong = Signer::new(
        wallet.address().clone(),
        crypto::Curve::Ed25519,
        SecretBytes::new([9; 32]),
    );
    let mut builder = factory.transaction(&Asset::native(network())).unwrap();
    builder
        .send(Transfer::new(recipient(), Decimal::from(100)).from(wrong))
        .unwrap();
    assert_eq!(
        block_on(builder.sign()).unwrap_err().kind,
        TransactionErrorKind::Failed
    );
    script.exhausted();
}

fn token() -> Asset {
    Asset {
        network: network(),
        id: MINT.to_owned(),
    }
}

#[test]
fn an_estimate_sums_signatures_and_a_token_accounts_rent() {
    let script = Script::new(signing_replies());
    script.push([
        Reply::ok("getMinimumBalanceForRentExemption", json!(2_039_280)).expecting(json!([165])),
    ]);
    let fees = config()
        .factory(&client(script.clone()), Arc::new(base::NoHoldings))
        .fees();
    let one = wallet(&script).address().clone();
    let two = recipient();

    let single =
        block_on(fees.estimate(&spends(&[(one.clone(), Asset::native(network()))]))).unwrap();
    assert_eq!(single, Decimal::from(5_000));

    let several = [
        (one.clone(), Asset::native(network())),
        (one, token()),
        (two, token()),
    ];
    let total = block_on(fees.estimate(&spends(&several))).unwrap();
    assert_eq!(total, Decimal::from(3 * 5_000 + 2 * 2_039_280));
    script.exhausted();
}

/// Holdings where every address holds every asset.
struct HeldByAll;

impl base::Holdings for HeldByAll {
    fn holds<'a>(
        &'a self,
        spends: &'a base::Spends,
    ) -> base::Future<'a, anyhow::Result<Vec<bool>>> {
        let held = spends.iter().map(|spend| spend.to.is_some()).collect();
        Box::pin(async move { Ok(held) })
    }
}

#[test]
fn a_token_sent_to_a_holder_is_priced_without_the_rent_of_its_account() {
    let script = Script::new(signing_replies());
    let fees = config()
        .factory(&client(script.clone()), Arc::new(HeldByAll))
        .fees();
    let one = wallet(&script).address().clone();
    let source = [(one, token())];
    let to_holder = block_on(fees.estimate(&spends_to(&source, &recipient()))).unwrap();
    assert_eq!(
        to_holder,
        Decimal::from(5_000),
        "one signature, no account to create"
    );
    // Without a recipient it is a new address: the rent is read and counted.
    script.push([Reply::ok(
        "getMinimumBalanceForRentExemption",
        json!(2_039_280),
    )]);
    let to_anyone = block_on(fees.estimate(&spends(&source))).unwrap();
    assert_eq!(to_anyone, Decimal::from(5_000 + 2_039_280));
    script.exhausted();
}

#[test]
fn prices_refresh_once_per_time_to_live() {
    let source = [(recipient(), Asset::native(network()))];
    let script = Script::new(signing_replies());
    let fees = config()
        .factory(&client(script.clone()), Arc::new(base::NoHoldings))
        .fees();
    block_on(fees.estimate(&spends(&source))).unwrap();
    block_on(fees.estimate(&spends(&source))).unwrap();
    script.exhausted();

    let mut config = config();
    config.fee_ttl_ms = 0;
    let script = Script::new(signing_replies());
    script.push(signing_replies());
    let fees = config
        .factory(&client(script.clone()), Arc::new(base::NoHoldings))
        .fees();
    block_on(fees.estimate(&spends(&source))).unwrap();
    block_on(fees.estimate(&spends(&source))).unwrap();
    script.exhausted();
}

#[test]
fn a_sweep_of_balance_less_estimate_pays_the_estimate() {
    let script = Script::new(signing_replies());
    let factory = config().factory(&client(script.clone()), Arc::new(base::NoHoldings));
    let wallet = block_on(factory.from_pk(SecretBytes::new(SEED))).unwrap();
    let source = [(wallet.address().clone(), Asset::native(network()))];
    let fee = block_on(factory.fees().estimate(&spends(&source))).unwrap();
    let mut builder = wallet.transaction(&Asset::native(network())).unwrap();
    builder
        .send((recipient(), Decimal::from(1_000_000) - fee.clone()))
        .unwrap();
    let signed = block_on(builder.sign()).unwrap();
    script.exhausted();
    let paid = signed.legs().find_map(|(_, _, leg)| match leg {
        Leg::Fee { amount, .. } => Some(amount.clone()),
        _ => None,
    });
    assert_eq!(paid, Some(fee), "the estimator's cached prices, no refresh");
}

#[test]
fn a_base58_secret_or_keypair_imports() {
    let factory = config().factory(&client(Script::new([])), Arc::new(base::NoHoldings));
    let keypair = Keypair::new_from_array(SEED);
    for text in [keypair.to_base58_string(), bs58::encode(SEED).into_string()] {
        let wallet = block_on(factory.from_pk_text(&text)).unwrap();
        assert_eq!(wallet.address().value, keypair.pubkey().to_string());
        assert_eq!(wallet.private_key().1.as_bytes(), SEED);
    }
    let mut forged = keypair.to_bytes();
    forged[63] ^= 1;
    for bad in [
        String::new(),
        "0OIl".to_owned(),
        bs58::encode(forged).into_string(),
    ] {
        let error = block_on(factory.from_pk_text(&bad)).err().unwrap();
        assert_eq!(
            error.downcast_ref::<WalletError>().map(|e| e.kind),
            Some(WalletErrorKind::InvalidSecret),
            "{bad:?}"
        );
    }
}
