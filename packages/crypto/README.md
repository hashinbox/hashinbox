# crypto

Secret key material and signing for secp256k1 (ECDSA, BIP-340 Schnorr) and
Ed25519. Secrets are zeroized on drop and have no `Clone`, `Debug` or Serde.

## Usage

```rust
use crypto::{
    Digest, KeyPair, PublicKeyFormat, SecretBytes, SignRequest, SignablePayload,
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
assert_eq!(signed.signature.bytes.len(), 65);
assert_eq!(signed.public_key.bytes.len(), 33);
# Ok(())
# }
```

An Ed25519 key is built with `KeyPair::ed25519` and answers the same
`SignRequest` with `SignablePayload::Message`, `SignatureScheme::Ed25519`,
`SignatureEncoding::Raw` and `PublicKeyFormat::Raw`.

## Notes

- A secp256k1 key signs the 32-byte digest it is given and does no hashing. The
  caller computes the protocol hash, because each chain hashes differently.
