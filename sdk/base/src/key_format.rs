//! [`KeyFormat`]: what the bytes of a stored private key are.

use std::{fmt, str::FromStr};

/// The exact bytes a stored private key holds — a property of the bytes, not
/// of the chain that uses them.
///
/// Import encodings (hex, base58, WIF, a JSON keypair) are normalized to one
/// of these before a key is stored; they are not formats of their own. A
/// wallet factory states the format of the keys it creates and imports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyFormat {
    /// A 32-byte secp256k1 scalar (Bitcoin, Ethereum).
    Secp256k1Scalar,
    /// A 32-byte Ed25519 seed (Solana).
    Ed25519Seed,
}

impl KeyFormat {
    /// The stored spelling: `secp256k1-scalar` or `ed25519-seed`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Secp256k1Scalar => "secp256k1-scalar",
            Self::Ed25519Seed => "ed25519-seed",
        }
    }
}

impl fmt::Display for KeyFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for KeyFormat {
    /// What was wrong: the text names no format.
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "secp256k1-scalar" => Ok(Self::Secp256k1Scalar),
            "ed25519-seed" => Ok(Self::Ed25519Seed),
            _ => Err("unknown private key format".to_owned()),
        }
    }
}
