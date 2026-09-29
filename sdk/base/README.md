# base: chain-neutral values and capabilities

## Why this crate exists

Every other SDK crate has to name the same things without naming a chain: which
network, which address, which asset, how much, which block, which transaction,
and "sign this" or "broadcast that". `base` is that shared vocabulary. Values
are canonical (string equality is identity), amounts are exact decimals, and
capabilities are small object-safe traits returning boxed futures, so the SDK
owns no async runtime.

Inspection lives inside the values those capabilities return rather than in
extra methods: [`TransactionBuilder::draft`] says what a transaction will do
before any node is asked, [`SignedTransaction::fee`] says what it costs
before anything leaves the process, so a caller with a ceiling drops the
builder and nothing happened, and [`SignedTransaction::claim`] names the
scarce things signing it took, in the chain's own spelling, so two signed
transactions can be compared with [`Claim::collides_with`] before either is
broadcast. A claim is a report, never a lock: preparing releases what it
reserved and broadcasting reserves it again.

It also owns [`SubmissionRegistrar`], the application's hook for supervising
send work that outlives the request that started it: it has no wallet content
and names no chain, so it belongs beside [`Broadcaster`] rather than in
`wallets`.

It deliberately does not own chain rules, native parsing, RPC, indexing,
wallets, storage, or a universal transaction model. Nor does it own
cryptography: `crypto` holds the key, the signing request and its result, and
`base` adds the one thing a package cannot — [`Signer`], the asynchronous
capability, because a key may live in an HSM, a KMS, or another process. A chain crate converts its
native types into these values in its `identity.rs` and keeps everything
protocol-specific to itself.

## Concepts

| Concept | Type | Provided by |
|---|---|---|
| chain family | [`ChainName`] | each chain's `CHAIN_NAME` constant |
| network (the namespace of every identity) | [`Network`], [`NetworkKind`], [`Networked`] | each chain's `identity.rs` implements [`Networked`] |
| well-known networks and assets | [`ChainNetwork`], [`ChainNetworks`], [`Asset`] | chain `native` constants |
| address | [`Address`] | chain `identity.rs` |
| asset identity | [`AssetId`] (`AssetId::native` for BTC/ETH/SOL) | chain crates, `wallets` |
| what an amount of an asset is | [`Unit`] (decimals, name, ticker) and [`Units`], keyed by [`AssetId`] | each chain's `Config`, folded by `wallets::Composition`; an edge resolves an amount here and nowhere else |
| amount | [`Decimal`] | re-exported from `decimal` |
| block coordinates | [`BlockPosition`], [`BlockHeight`], [`BlockHash`], [`BlockParent`], [`BlockRef`] | chain sources |
| parsed block | [`Block`], [`Transactions`], [`Identified`] | chain `native::block` |
| value change | [`MovementKind`] | chain interpreters, `indexing` history |
| transaction identity | [`TransactionId`], [`TransactionRef`] | chain `identity.rs` |
| signing | [`Signer`], [`SignRequest`], [`SignedPayload`] | chain `key.rs`; the in-process key is [`crypto::KeyPair`] on either curve, which this crate implements [`Signer`] for |
| building and sending | [`TransactionBuilder`], [`Draft`], [`Transfer`], [`SignedTransaction`], [`Claim`], [`TransactionSnapshot`], [`Broadcaster`], [`Submission`] | chain `builder.rs`, `wallet.rs` |
| supervising background send work | [`SubmissionRegistrar`], [`SubmissionTask`], [`Activation`], [`RegistrationError`] | the application supervises; chains hand tasks over (`wallets` re-exports them) |
| crypto values | [`PublicKey`], [`SecretBytes`], [`Signature`], [`SignatureScheme`], [`ScalarTweak`], [`Digest`], [`SignablePayload`], … | re-exported from `crypto` |

## Public API map

| Group | Items |
|---|---|
| construct | [`Network::new`], [`Address::new`], [`AssetId::native`], [`Unit::native`], [`Unit::token`], `Units::from_iter`, [`Decimal::zero`], [`Decimal::from_atomic`], `"1.5".parse::<Decimal>()`, [`TransactionRef::new`], [`crypto::KeyPair::secp256k1`], [`crypto::KeyPair::ed25519`], [`Block::new`], [`Transactions::new`] |
| use | [`Networked::canonical`], [`Address::belongs_to`], [`ChainNetworks::testnet`], [`Units::of`], [`Unit::display`], [`Decimal::to_atomic`], [`Decimal::to_atomic_u64`], [`Decimal::validate_amount`], [`Transactions::get`], [`SignedTransaction::fee`], [`SignedTransaction::claim`], [`Claim::collides_with`], [`SubmissionTask::dormant`], [`crypto::KeyPair::public_key`] |
| implement | [`Networked`], [`Signer`], [`TransactionBuilder`], [`Broadcaster`], [`SubmissionRegistrar`], [`Identified`] |
| async | [`BoxFuture`], [`SignFuture`], [`TransactionFuture`] |
| errors | [`SignerError`] and [`TransactionError`] with `…Kind` enums; [`DecimalError`] with one message |

## Usage

```rust
use base::{
    Address, Asset, AssetId, ChainName, ChainNetwork, Decimal, Digest, Network, NetworkKind,
    PublicKeyFormat, SignRequest, SignablePayload, SignatureEncoding, SignatureScheme, Signer,
    Unit, Units,
};
use crypto::KeyPair;

const CHAIN: ChainName = ChainName::from_static("example");

# fn main() -> Result<(), Box<dyn std::error::Error>> {
// Identities are canonical text inside one network.
let network = Network::new(CHAIN, "testnet");
let address = Address::new(network.clone(), "addr1");
assert!(address.belongs_to(&network));
assert!(AssetId::native(network.chain.clone()).is_native());

// Amounts are exact: display units <-> atomic units, never floats.
let amount: Decimal = "0.015".parse()?;
assert_eq!(amount.to_atomic_u64(8)?, 1_500_000);
assert_eq!(Decimal::from_atomic(1_500_000_u32.into(), 8), amount);

// What an amount *is* belongs to its asset, resolved by identity: a chain
// declares its own currency, and a token declares only its own scale.
const COIN: Asset = Asset::new(
    ChainNetwork::new(CHAIN, "testnet", NetworkKind::Testnet, "EXM"),
    "Examplecoin",
    "EXM",
    8,
);
let token = AssetId { chain: CHAIN, asset: "0xc0ffee".to_owned() };
let units = Units::from_iter([
    (AssetId::native(CHAIN), Unit::native(&COIN)),
    (token.clone(), Unit::token(6)),
]);
let unit = units.of(&token).ok_or("the token's declared unit")?;
assert_eq!(unit.display(&Decimal::from(1_500_000_u64))?, "1.5".parse()?);
let coin = units.of(&AssetId::native(CHAIN)).ok_or("the currency's unit")?;
assert_eq!((coin.decimals, coin.ticker.as_deref()), (8, Some("EXM")));

// A signer signs what the chain asks for; it never sees a transaction.
let key = KeyPair::secp256k1([1_u8; 32])?;
let signed = futures_executor::block_on(key.sign(SignRequest {
    payload: SignablePayload::Digest(Digest { bytes: vec![9; 32] }),
    scheme: SignatureScheme::EcdsaSecp256k1,
    encoding: SignatureEncoding::Recoverable,
    public_key_format: PublicKeyFormat::Compressed,
    key_tweak: None,
}))?;
assert_eq!(signed.signature.bytes.len(), 65);
# Ok(())
# }
```

## Related crates

| Layer (`linter.toml`) | Crates | Relation |
|---|---|---|
| packages | `crypto`, `decimal` | secret keys, the signing vocabulary ([`SignRequest`], [`SignedPayload`], [`SignerError`]), the in-process [`crypto::KeyPair`], and the exact amount this crate re-exports; [`DecimalError`] is `decimal`'s `Error` |
| indexing | `indexing` | builds history, checkpoints, and outputs from these values |
| wallets | `wallets` | wallet capabilities over [`Address`], [`Decimal`], and [`TransactionBuilder`] |
| chains | `chain-bitcoin`, `chain-ethereum`, `chain-solana` | map native types to these values and implement the traits |

Design notes: [`ARCHITECTURE.md`](../../ARCHITECTURE.md),
[`docs/CHAIN_LAYOUT.md`](../../docs/CHAIN_LAYOUT.md),
[`docs/INDEXING.md`](../../docs/INDEXING.md).
