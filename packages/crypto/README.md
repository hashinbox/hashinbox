# crypto: secret key material and chain-neutral signing

## Why this crate exists

Every chain needs the same few cryptographic mechanics: generate or accept
secret bytes without leaking them, derive a public key in a given encoding,
and sign — a 32-byte digest with ECDSA or BIP-340 Schnorr on secp256k1
(optionally with a Taproot-style key tweak), or the exact message with
Ed25519. This crate owns exactly that, with zeroization on drop and no
`Clone`, `Debug`, or Serde on secrets. It also owns the vocabulary that
describes one signing job ([`SignRequest`], [`SignedPayload`],
[`SignerError`]) and [`KeyPair`], the in-process key that answers one.

[`KeyPair`] is the signing engine of every chain in the repository, so no
chain reaches for a signing library of its own: Bitcoin and Ethereum import a
secp256k1 scalar with [`KeyPair::secp256k1`], Solana an Ed25519 seed with
[`KeyPair::ed25519`], and all three then answer the same [`SignRequest`]. The
curve decides what a request may ask for, and nothing else does.

It does not own protocol hashing (on secp256k1 the caller supplies the
digest), domain-separation tags, transaction rules, address formats, wallets,
RPC, or custody policy. It owns no asynchrony either: a key that lives in an
HSM or another process is a capability, and `base::Signer` is where that
capability is declared.

## Concepts

| Concept | Type | Notes |
|---|---|---|
| secret bytes | [`SecretBytes`] | opaque, zeroized; passed from composition into a key owner. [`SecretBytes::from_hex`] is the *one* secret-text parse in the repository: every supported chain writes a key as 64 hexadecimal characters, so no wallet factory owns a parse of its own |
| secret key | private to [`KeyPair`], one type per curve | a secp256k1 scalar validated against the group order, or an Ed25519 seed; both exactly 32 bytes, because a curve left-pads or truncates a wrong-length slice into a key the caller does not control, so the length is checked here and no caller repeats it |
| public key | [`PublicKey`] with [`Curve`] and [`PublicKeyFormat`] | plain value, serializable |
| key tweak | [`ScalarTweak`] | added to the key before Schnorr signing; ECDSA and Ed25519 refuse a tweaked request |
| signature | [`Signature`] with [`SignatureScheme`] and [`SignatureEncoding`] | plain value, serializable |
| signing job | [`SignRequest`] over a [`SignablePayload`] ([`Digest`] or message), answered by a [`SignedPayload`] | the shape every signer speaks, local or remote |
| in-process key | [`KeyPair`] | one secret, on one [`Curve`], that answers a [`SignRequest`] with [`KeyPair::sign_now`] |
| failure | [`Error`] with [`ErrorKind`]; [`SignerError`] with [`SignerErrorKind`] | a primitive that failed, and a request that was refused |

## Public API map

| Group | Items |
|---|---|
| construct | [`SecretBytes::generate_secp256k1`], [`SecretBytes::generate_ed25519`], [`SecretBytes::new`], [`SecretBytes::from_hex`], [`KeyPair::secp256k1`], [`KeyPair::ed25519`] |
| use | [`SecretBytes::as_bytes`], [`KeyPair::public_key`], [`KeyPair::sign_now`] |
| values | [`PublicKey`], [`Signature`], [`Curve`], [`PublicKeyFormat`], [`SignatureScheme`], [`SignatureEncoding`], [`ScalarTweak`], [`SignRequest`], [`SignablePayload`], [`Digest`], [`SignedPayload`] |
| implement | nothing |
| errors | [`Error`], [`ErrorKind`], [`SignerError`], [`SignerErrorKind`] |

## Usage

A secp256k1 key signs a digest its caller already computed:

```rust
use crypto::{
    Curve, Digest, KeyPair, PublicKeyFormat, SecretBytes, SignRequest, SignablePayload,
    SignatureEncoding, SignatureScheme,
};

# fn main() -> Result<(), crypto::SignerError> {
let secret = SecretBytes::generate_secp256k1().expect("OS randomness");
let key = KeyPair::secp256k1(secret.as_bytes())?;

let signed = key.sign_now(SignRequest {
    payload: SignablePayload::Digest(Digest { bytes: vec![9; 32] }),
    scheme: SignatureScheme::EcdsaSecp256k1,
    encoding: SignatureEncoding::Recoverable,
    public_key_format: PublicKeyFormat::Compressed,
    key_tweak: None,
})?;
assert_eq!(signed.signature.bytes.len(), 65); // r || s || recovery id
assert_eq!(signed.public_key.curve, Curve::Secp256k1);
assert_eq!(signed.public_key.bytes.len(), 33);
# Ok(())
# }
```

An Ed25519 key answers the same [`SignRequest`], but signs the exact message
and has one public-key and signature shape:

```rust
use crypto::{
    Curve, KeyPair, PublicKeyFormat, SignRequest, SignablePayload, SignatureEncoding,
    SignatureScheme,
};

# fn main() -> Result<(), crypto::SignerError> {
let key = KeyPair::ed25519([7_u8; 32])?;

let signed = key.sign_now(SignRequest {
    payload: SignablePayload::Message(b"the exact wire message".to_vec()),
    scheme: SignatureScheme::Ed25519,
    encoding: SignatureEncoding::Raw,
    public_key_format: PublicKeyFormat::Raw,
    key_tweak: None,
})?;
assert_eq!(signed.signature.bytes.len(), 64);
assert_eq!(signed.public_key.curve, Curve::Ed25519);
assert_eq!(signed.public_key.bytes.len(), 32);
# Ok(())
# }
```

## Related crates

| Layer (`linter.toml`) | Crates | Relation |
|---|---|---|
| packages | none | depends only on `k256`, `ed25519-dalek`, `sha2`, `getrandom`, `zeroize` |
| base | `base` | re-exports the value types and implements its asynchronous `Signer` for [`KeyPair`] |
| wallets, chains | `wallets`, `chain-*` | pass [`SecretBytes`] into wallet factories, decoding caller text with [`SecretBytes::from_hex`]; the Bitcoin, Ethereum, and Solana keys all wrap a [`KeyPair`], whose constructors are where a wrong length is refused |

Design notes: [`ARCHITECTURE.md`](../../ARCHITECTURE.md),
[`docs/SYSTEM_REQUIREMENTS.md`](../../docs/SYSTEM_REQUIREMENTS.md).
