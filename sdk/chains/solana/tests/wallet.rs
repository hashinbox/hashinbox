//! Wallets, building and signing, and broadcast, over scripted nodes.

use std::sync::Arc;

use ::rpc::testing::{Reply, Script};
use base::{
    Asset, Broadcaster as _, Decimal, Fee, FeeLevel, Leg, Signer, TransactionErrorKind,
    TransactionState, Transfer, Wallet, WalletError, WalletErrorKind, WalletFactory as _,
};
use chain_solana::{Client, Config, Factory};
use crypto::SecretBytes;
use futures_executor::block_on;
use serde_json::json;
use solana_keypair::{Keypair, Signer as _};
use solana_transaction::Transaction;

/// `transfer`, signed by `builder`.
async fn pay(
    mut builder: Box<dyn base::Builder>,
    transfer: Transfer,
) -> Result<base::Transaction, base::TransactionError> {
    builder.from_transfer(transfer)?;
    builder.sign().await
}

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
    let factory =
        Factory::from(config()).wallets(&client(script.clone()), Arc::new(base::NoHoldings));
    block_on(factory.from_pk(SecretBytes::new(SEED))).unwrap()
}

/// An on-curve recipient: the address of another key.
fn recipient() -> base::Address {
    let key = Keypair::new_from_array([8; 32]);
    base::Address::new(network(), key.pubkey().to_string())
}

/// A blockhash (of 3s, valid through height 50), a signature at 5 000
/// lamports, and the recent priority fees.
fn signing_replies() -> [Reply; 3] {
    replies(3, 50)
}

/// [`signing_replies`] on blockhash `blockhash` (of that byte), valid
/// through height `expires`.
fn replies(blockhash: u8, expires: u64) -> [Reply; 3] {
    [
        Reply::ok(
            "getLatestBlockhash",
            json!({ "context": { "slot": 1 }, "value": { "blockhash": base58(blockhash, 32), "lastValidBlockHeight": expires } }),
        ),
        Reply::ok(
            "getFeeForMessage",
            json!({ "context": { "slot": 1 }, "value": 5000 }),
        ),
        recent(),
    ]
}

/// Recent priority fees of 11, 21, … 1 001 micro-lamports per compute
/// unit, one per slot, unordered: 501 at the median.
fn recent() -> Reply {
    let fees: Vec<_> = (1..=100_u64)
        .rev()
        .map(|slot| json!({ "slot": slot, "prioritizationFee": slot * 10 + 1 }))
        .collect();
    Reply::ok("getRecentPrioritizationFees", json!(fees))
}

/// Lamports a SOL transfer pays at `Normal`: a signature and
/// `ceil(501 × 40 000 / 1 000 000)`.
const SOL_FEE: u64 = 5_000 + 21;
/// Lamports a token transfer pays at `Normal`: a signature and
/// `ceil(501 × 100 000 / 1 000 000)`.
const TOKEN_FEE: u64 = 5_000 + 51;

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

    let factory = Factory::from(config()).wallets(&client(script), Arc::new(base::NoHoldings));
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
fn a_sol_transfer_signs_one_verifying_transaction_with_its_legs() {
    let script = Script::new(signing_replies());
    let wallet = wallet(&script);
    let sol = Asset::native(network());
    let builder = wallet.transaction(&sol).unwrap();

    let transaction =
        block_on(pay(builder, Transfer::new(recipient(), Decimal::from(100)))).unwrap();

    let decoded: Transaction = bincode::deserialize(&transaction.bytes).unwrap();
    decoded.verify().unwrap();
    assert_eq!(
        decoded.message.instructions.len(),
        4,
        "the compute-unit limit and price, the transfer and a memo"
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
                amount: Decimal::from(SOL_FEE)
            },
            Leg::Spent {
                address: from,
                asset: sol.clone(),
                amount: Decimal::from(100),
                coin: None
            },
            Leg::Received {
                address: recipient(),
                asset: sol,
                amount: Decimal::from(100),
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
    let builder = wallet.transaction(&usdc).unwrap();

    let signed = block_on(pay(builder, Transfer::new(recipient(), Decimal::from(20)))).unwrap();

    let decoded: Transaction = bincode::deserialize(&signed.bytes).unwrap();
    let programs: Vec<String> = decoded
        .message
        .instructions
        .iter()
        .map(|instruction| {
            decoded.message.account_keys[usize::from(instruction.program_id_index)].to_string()
        })
        .collect();
    assert_eq!(
        programs[2..4],
        [
            "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        ]
    );
    let mut data = vec![12];
    data.extend_from_slice(&20_u64.to_le_bytes());
    data.push(6);
    assert_eq!(decoded.message.instructions[3].data, data);
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
        accounts(2)[..4],
        [
            from.clone(),
            receiver.clone(),
            recipient().value,
            MINT.to_owned()
        ],
        "create the recipient's token account, paid by the sender"
    );
    assert_eq!(accounts(3), [sender, MINT.to_owned(), receiver, from]);
    assert_eq!(
        signed.legs[1..],
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
    // Each refused before any I/O.
    let refused = |to: base::Address, amount: Decimal| {
        let builder = wallet.transaction(&Asset::native(network())).unwrap();
        block_on(pay(builder, Transfer::new(to, amount)))
            .unwrap_err()
            .kind
    };
    assert_eq!(
        refused(recipient(), Decimal::zero()),
        TransactionErrorKind::Failed
    );
    assert_eq!(
        refused(recipient(), "0.5".parse().unwrap()),
        TransactionErrorKind::Failed
    );
    assert_eq!(
        refused(wallet.address().clone(), Decimal::from(1)),
        TransactionErrorKind::Failed
    );
    let off_curve = base::Address::new(network(), associated(&wallet.address().value).to_string());
    assert_eq!(
        refused(off_curve, Decimal::from(1)),
        TransactionErrorKind::Failed
    );
    script.exhausted();
}

fn signed(script: &Script) -> base::Transaction {
    let wallet = wallet(script);
    let builder = wallet.transaction(&Asset::native(network())).unwrap();
    block_on(pay(builder, Transfer::new(recipient(), Decimal::from(100)))).unwrap()
}

#[test]
fn a_send_expires_at_its_blockhashs_last_valid_height() {
    let transaction = signed(&Script::new(signing_replies()));
    assert_eq!(transaction.expires_height, Some(50));
}

#[test]
fn broadcast_sends_the_exact_bytes_once() {
    let transaction = signed(&Script::new(signing_replies()));
    let node = Script::new([Reply::ok("sendTransaction", json!(transaction.id.id))])
        .once_only("sendTransaction");
    let broadcaster = client(node.clone());

    block_on(broadcaster.broadcast(&transaction)).unwrap();

    let requests = node.requests();
    let encoded = requests[0].1[0].as_str().unwrap();
    assert_eq!(base64_decode(encoded), transaction.bytes);
    node.exhausted();
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
fn a_transfer_from_its_own_source_is_paid_and_signed_by_it() {
    let script = Script::new([]);
    let factory =
        Factory::from(config()).wallets(&client(script.clone()), Arc::new(base::NoHoldings));
    let one = block_on(factory.from_pk(SecretBytes::new(SEED))).unwrap();
    let two = block_on(factory.from_pk(SecretBytes::new([9; 32]))).unwrap();
    // One's builder, paid by two.
    let builder = one.transaction(&Asset::native(network())).unwrap();
    script.push(signing_replies());
    let transfer = Transfer::new(recipient(), Decimal::from(100)).from(&two);
    let transaction = block_on(pay(builder, transfer)).unwrap();
    script.exhausted();
    let decoded: Transaction = bincode::deserialize(&transaction.bytes).unwrap();
    decoded.verify().unwrap();
    assert_eq!(
        decoded.message.account_keys[0].to_string(),
        two.address().value,
        "paid and signed by its source"
    );
}

#[test]
fn a_factory_builder_needs_a_source_whose_key_signs_for_it() {
    let script = Script::new([]);
    let factory =
        Factory::from(config()).wallets(&client(script.clone()), Arc::new(base::NoHoldings));
    let builder = factory.transaction(&Asset::native(network())).unwrap();
    let transfer = Transfer::new(recipient(), Decimal::from(100));
    assert_eq!(
        block_on(pay(builder, transfer)).unwrap_err().kind,
        TransactionErrorKind::Failed
    );

    let wallet = wallet(&script);
    let wrong = Signer::new(
        wallet.address().clone(),
        crypto::Curve::Ed25519,
        SecretBytes::new([9; 32]),
    );
    let builder = factory.transaction(&Asset::native(network())).unwrap();
    let transfer = Transfer::new(recipient(), Decimal::from(100)).from(wrong);
    assert_eq!(
        block_on(pay(builder, transfer)).unwrap_err().kind,
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
fn an_estimate_sums_signatures_priority_fees_and_a_token_accounts_rent() {
    let script = Script::new(signing_replies());
    // The rent, asked once.
    script.push([
        Reply::ok("getMinimumBalanceForRentExemption", json!(2_039_280)).expecting(json!([165])),
    ]);
    let fees = Factory::from(config())
        .wallets(&client(script.clone()), Arc::new(base::NoHoldings))
        .fees();
    let one = wallet(&script).address().clone();
    let two = recipient();

    let single =
        block_on(fees.estimate(&spends(&[(one.clone(), Asset::native(network()))]))).unwrap();
    assert_eq!(single, Decimal::from(SOL_FEE));

    let several = [
        (one.clone(), Asset::native(network())),
        (one, token()),
        (two, token()),
    ];
    let total = block_on(fees.estimate(&spends(&several))).unwrap();
    assert_eq!(
        total,
        Decimal::from(SOL_FEE + 2 * TOKEN_FEE + 2 * 2_039_280)
    );
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
    let fees = Factory::from(config())
        .wallets(&client(script.clone()), Arc::new(HeldByAll))
        .fees();
    let one = wallet(&script).address().clone();
    let source = [(one, token())];
    let to_holder = block_on(fees.estimate(&spends_to(&source, &recipient()))).unwrap();
    assert_eq!(
        to_holder,
        Decimal::from(TOKEN_FEE),
        "one signature and its priority, no account to create"
    );
    // Without a recipient it is a new address: the rent is read and counted.
    script.push([Reply::ok(
        "getMinimumBalanceForRentExemption",
        json!(2_039_280),
    )]);
    let to_anyone = block_on(fees.estimate(&spends(&source))).unwrap();
    assert_eq!(to_anyone, Decimal::from(TOKEN_FEE + 2_039_280));
    script.exhausted();
}

#[test]
fn prices_refresh_once_per_time_to_live() {
    let source = [(recipient(), Asset::native(network()))];
    let script = Script::new(signing_replies());
    let fees = Factory::from(config())
        .wallets(&client(script.clone()), Arc::new(base::NoHoldings))
        .fees();
    block_on(fees.estimate(&spends(&source))).unwrap();
    block_on(fees.estimate(&spends(&source))).unwrap();
    script.exhausted();

    let mut config = config();
    config.fee_ttl_ms = 0;
    let script = Script::new(signing_replies());
    script.push(signing_replies());
    let fees = Factory::from(config)
        .wallets(&client(script.clone()), Arc::new(base::NoHoldings))
        .fees();
    block_on(fees.estimate(&spends(&source))).unwrap();
    block_on(fees.estimate(&spends(&source))).unwrap();
    script.exhausted();
}

#[test]
fn a_sweep_of_balance_less_estimate_pays_the_estimate() {
    let script = Script::new(signing_replies());
    let factory =
        Factory::from(config()).wallets(&client(script.clone()), Arc::new(base::NoHoldings));
    let wallet = block_on(factory.from_pk(SecretBytes::new(SEED))).unwrap();
    let source = [(wallet.address().clone(), Asset::native(network()))];
    let fee = block_on(factory.fees().estimate(&spends_to(&source, &recipient()))).unwrap();
    // A fresh blockhash to sign on; the prices kept.
    script.push([Reply::ok(
        "getLatestBlockhash",
        json!({ "context": { "slot": 1 }, "value": { "blockhash": base58(3, 32), "lastValidBlockHeight": 50 } }),
    )]);
    let builder = wallet.transaction(&Asset::native(network())).unwrap();
    let transfer = Transfer::new(recipient(), Decimal::from(1_000_000) - fee.clone());
    let signed = block_on(pay(builder, transfer)).unwrap();
    script.exhausted();
    let paid = signed.legs.iter().find_map(|leg| match leg {
        Leg::Fee { amount, .. } => Some(amount.clone()),
        _ => None,
    });
    assert_eq!(paid, Some(fee), "the estimator's cached prices");
}

#[test]
fn a_base58_secret_or_keypair_imports() {
    let factory =
        Factory::from(config()).wallets(&client(Script::new([])), Arc::new(base::NoHoldings));
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

/// The compute-unit limit and price `signed` carries: its first two
/// instructions, `SetComputeUnitLimit` and `SetComputeUnitPrice`.
fn priority(signed: &base::Transaction) -> (u32, u64) {
    let decoded: Transaction = bincode::deserialize(&signed.bytes).unwrap();
    let message = &decoded.message;
    let budget = |index: usize, tag: u8| {
        let instruction = &message.instructions[index];
        let program = message.account_keys[usize::from(instruction.program_id_index)];
        assert_eq!(
            program.to_string(),
            "ComputeBudget111111111111111111111111111111"
        );
        assert_eq!(instruction.data[0], tag);
        instruction.data[1..].to_vec()
    };
    let (limit, price) = (budget(0, 2), budget(1, 3));
    (
        u32::from_le_bytes(limit.try_into().unwrap()),
        u64::from_le_bytes(price.try_into().unwrap()),
    )
}

/// The blockhash `signed` is on.
fn blockhash(signed: &base::Transaction) -> [u8; 32] {
    let decoded: Transaction = bincode::deserialize(&signed.bytes).unwrap();
    decoded.message.recent_blockhash.to_bytes()
}

#[test]
fn each_fee_level_pays_its_multiple_of_the_median_recent_priority_fee() {
    // The price (half, once or twice 501, rounded up), and its fee at
    // 40 000 units, rounded up.
    let levels = [
        (FeeLevel::Low, 251, 11),
        (FeeLevel::Normal, 501, 21),
        (FeeLevel::High, 1002, 41),
    ];
    for (level, price, lamports) in levels {
        let script = Script::new(signing_replies());
        let wallet = wallet(&script);
        let mut builder = wallet.transaction(&Asset::native(network())).unwrap();
        builder
            .from_transfer(Transfer::new(recipient(), Decimal::from(100)))
            .unwrap();
        builder.with_fee(Fee::Set(level));
        let signed = block_on(builder.sign()).unwrap();
        script.exhausted();
        assert_eq!(priority(&signed), (40_000, price), "{level:?}");
        assert_eq!(
            signed.legs[0],
            Leg::Fee {
                address: wallet.address().clone(),
                asset: Asset::native(network()),
                amount: Decimal::from(5_000 + lamports)
            },
            "a signature and its priority fee"
        );
        assert_eq!(
            script.requests()[2].1,
            json!([]),
            "priced on the whole network"
        );
    }
}

#[test]
fn a_price_above_max_priority_fee_is_refused_as_fee_cap() {
    let mut config = config();
    config.max_priority_fee = 500;
    let script = Script::new(signing_replies());
    let factory = Factory::from(config).wallets(&client(script), Arc::new(base::NoHoldings));
    let wallet = block_on(factory.from_pk(SecretBytes::new(SEED))).unwrap();
    let builder = wallet.transaction(&Asset::native(network())).unwrap();
    let error = block_on(pay(builder, Transfer::new(recipient(), Decimal::from(100))));
    assert_eq!(
        error.unwrap_err().kind,
        TransactionErrorKind::FeeCap,
        "501 at Normal"
    );
}

/// `amount` of `asset` to the recipient, signed, and its builder's state.
fn signed_and_saved(asset: &Asset, amount: u64) -> (base::Transaction, serde_json::Value) {
    let script = Script::new(signing_replies());
    let mut builder = wallet(&script).transaction(asset).unwrap();
    let transfer = Transfer::new(recipient(), Decimal::from(amount));
    builder.from_transfer(transfer).unwrap();
    let signed = block_on(builder.sign()).unwrap();
    (signed, builder.save().unwrap())
}

/// `state`, loaded as saved into a new wallet's builder of `asset`, signed
/// at `fee` on blockhash 4 valid through height 60.
fn continued(state: &serde_json::Value, asset: &Asset, fee: Fee) -> base::Transaction {
    let script = Script::new(replies(4, 60));
    let mut builder = wallet(&script).transaction(asset).unwrap();
    builder.load(state).unwrap();
    assert_eq!(&builder.save().unwrap(), state, "the round trip");
    builder.with_fee(fee);
    let next = block_on(builder.sign()).unwrap();
    script.exhausted();
    next
}

#[test]
fn a_continued_transfer_pays_the_same_a_tenth_up_on_a_new_blockhash() {
    for asset in [Asset::native(network()), token()] {
        let (signed, state) = signed_and_saved(&asset, 20);
        let next = continued(&state, &asset, Fee::default());
        let ((units, price), (old_units, old_price)) = (priority(&next), priority(&signed));
        assert_eq!(units, old_units, "the same compute-unit limit");
        assert!(price > old_price, "a tenth up: {price} over {old_price}");
        assert_ne!(next.id, signed.id);
        assert_eq!(blockhash(&next), [4; 32], "a new blockhash");
        assert_eq!(next.expires_height, Some(60));
    }
}

#[test]
fn a_bump_pays_a_tenth_more_or_todays_price_when_higher() {
    let sol = Asset::native(network());
    let (_, state) = signed_and_saved(&sol, 100);
    // It paid 501: a tenth more, rounded up, is 552; `High` today is 1002.
    for (fee, price) in [(Fee::Times(1.1), 552), (Fee::Set(FeeLevel::High), 1002)] {
        let next = continued(&state, &sol, fee);
        assert_eq!(priority(&next), (40_000, price));
        assert_eq!(blockhash(&next), [4; 32], "a new blockhash");
    }
}
