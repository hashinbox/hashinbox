use sha2::{Digest as _, Sha256};

use crate::{Error, ErrorKind};

/// Elliptic curve a public key belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Curve {
    /// secp256k1 (Bitcoin, Ethereum).
    Secp256k1,
    /// Curve25519 in Edwards form (Solana).
    Ed25519,
}

/// Byte layout of a public key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PublicKeyFormat {
    /// 33-byte SEC1 `02/03 || x` (Bitcoin P2WPKH).
    Compressed,
    /// 65-byte SEC1 `04 || x || y`.
    Uncompressed,
    /// 32-byte `x` (BIP-340, Taproot).
    XOnly,
    /// The curve's plain bytes: 64-byte `x || y` on secp256k1, 32 bytes on Ed25519.
    Raw,
}

/// Public key bytes labelled with their curve and layout.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PublicKey {
    /// Curve of the key.
    pub curve: Curve,
    /// Layout of `bytes`.
    pub format: PublicKeyFormat,
    /// The encoded key.
    pub bytes: Vec<u8>,
}

/// A scalar added to the secret key before Schnorr signing.
///
/// BIP-340 Schnorr is the only scheme that accepts one: ECDSA and Ed25519
/// refuse a tweaked request rather than ignoring the tweak.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ScalarTweak {
    /// Adds this 32-byte big-endian scalar.
    Add([u8; 32]),
    /// Adds the BIP-340 tagged hash `H_tag(x-only public key || suffix)`, the
    /// BIP-341 Taproot output-key tweak when `tag` is `TapTweak`.
    TaggedHashAdd {
        /// Hash tag; must not be empty.
        tag: Vec<u8>,
        /// Bytes after the public key (a Taproot merkle root, or empty).
        suffix: Vec<u8>,
    },
}

impl ScalarTweak {
    /// The scalar to add, given the untweaked x-only public key it commits to.
    pub(crate) fn scalar(&self, public_key: &[u8]) -> Result<[u8; 32], Error> {
        match self {
            Self::Add(bytes) => Ok(*bytes),
            Self::TaggedHashAdd { tag, suffix } => {
                if tag.is_empty() {
                    return Err(Error::new(ErrorKind::InvalidTweak, "tag must not be empty"));
                }
                let tag_hash = Sha256::digest(tag);
                let mut hash = Sha256::new();
                hash.update(tag_hash);
                hash.update(tag_hash);
                hash.update(public_key);
                hash.update(suffix);
                Ok(hash.finalize().into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tagged_hash_tweak_commits_to_the_public_key_and_rejects_an_empty_tag() {
        let public = [7_u8; 32];
        let tweak = ScalarTweak::TaggedHashAdd {
            tag: b"TapTweak".to_vec(),
            suffix: Vec::new(),
        };
        let expected = {
            let tag_hash = Sha256::digest(b"TapTweak");
            let mut hash = Sha256::new();
            hash.update(tag_hash);
            hash.update(tag_hash);
            hash.update(public);
            <[u8; 32]>::from(hash.finalize())
        };
        assert_eq!(tweak.scalar(&public), Ok(expected));
        assert_eq!(ScalarTweak::Add([3; 32]).scalar(&public), Ok([3; 32]));
        assert_eq!(
            ScalarTweak::TaggedHashAdd {
                tag: Vec::new(),
                suffix: Vec::new(),
            }
            .scalar(&public),
            Err(Error::new(ErrorKind::InvalidTweak, "tag must not be empty"))
        );
    }
}
