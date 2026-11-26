# crypto

Secret keys and signing, one type per scheme: `Secp256k1Ecdsa`,
`Secp256k1Schnorr` (BIP-340; `tweaked` derives the BIP-341 output key) and
`Ed25519`. Keys are zeroized on drop by their libraries and have no `Clone` or
Serde; `Debug` is redacted. `PrivateKey` is what all share: `generate`,
`from_bytes` (exactly 32 bytes), `to_bytes` into `SecretBytes`, and `sign`
returning the scheme's wire signature.

Beside the keys, the pieces a server's logins need, each a public module or
function:

| Item | What |
|---|---|
| `password::{hash, verify, DECOY}` | argon2id PHC hashes; `DECOY` is checked when no account matches, so a wrong name costs a password check's time |
| `totp::{code, verify, uri, STEP}` | RFC 6238 codes (HMAC-SHA1, 30 s, 6 digits); `verify` accepts ±1 step after `last_step` only; `uri` is the `otpauth://` QR text |
| `mnemonic::Mnemonic` | BIP39, 12 English words: `generate`, `from_entropy`, `parse` (any whitespace or case; known words, valid checksum), `Display` the normal form |
| `base58::{encode, decode}` | Bitcoin's alphabet, no check bytes |
| `random::<N>()`, `sha256`, `crc32` | OS randomness, SHA-256, CRC32 (ISO-HDLC) |

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
