# crypto

Secret keys and signing, one type per scheme: `Secp256k1Ecdsa`,
`Secp256k1Schnorr` (BIP-340; `tweaked` derives the BIP-341 output key) and
`Ed25519`. Keys are zeroized on drop by their libraries and have no `Clone` or
Serde; `Debug` is redacted. `PrivateKey` is what all share: `generate`,
`from_bytes` (exactly 32 bytes), `to_bytes` into `SecretBytes`, and `sign`
returning the scheme's wire signature.

## Usage

```rust
use crypto::{Ed25519, PrivateKey, Secp256k1Ecdsa, Secp256k1Schnorr};

# fn main() -> Result<(), crypto::Error> {
let key = Secp256k1Ecdsa::from_bytes(Secp256k1Ecdsa::generate()?.to_bytes().as_bytes())?;
assert_eq!(key.sign(&[9; 32])?.len(), 65); // r || s || recovery id
assert_eq!(key.compressed_public_key().len(), 33);

let taproot = Secp256k1Schnorr::from_bytes(key.to_bytes().as_bytes())?.tweaked(None)?;
assert_eq!(taproot.sign(&[9; 32])?.len(), 64);

let key = Ed25519::generate()?;
assert_eq!(key.sign(b"message")?.len(), 64);
# Ok(())
# }
```

## Notes

- A secp256k1 key signs the 32-byte digest it is given and does no hashing. The
  caller computes the protocol hash, because each chain hashes differently.
