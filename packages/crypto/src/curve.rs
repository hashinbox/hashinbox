/// Elliptic curve of a key; how a store files which key type its bytes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Curve {
    /// secp256k1 (Bitcoin, Ethereum).
    Secp256k1,
    /// Curve25519 in Edwards form (Solana).
    Ed25519,
}
