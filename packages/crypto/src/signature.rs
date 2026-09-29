/// Signature algorithm and curve, named so signed data is self-describing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SignatureScheme {
    /// ECDSA over secp256k1 (Bitcoin legacy/SegWit v0, Ethereum).
    EcdsaSecp256k1,
    /// BIP-340 Schnorr over secp256k1 (Bitcoin Taproot).
    SchnorrSecp256k1,
    /// Ed25519 (Solana).
    Ed25519,
}

/// Byte layout of a signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SignatureEncoding {
    /// ASN.1 DER (ECDSA).
    Der,
    /// 64-byte `r || s` (ECDSA).
    Compact,
    /// 65-byte `r || s || recovery id` (ECDSA, Ethereum).
    Recoverable,
    /// The scheme's native bytes (64-byte Schnorr or Ed25519, `r || s` for ECDSA).
    Raw,
}

/// Signature bytes labelled with how they were produced.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Signature {
    /// Algorithm that produced `bytes`.
    pub scheme: SignatureScheme,
    /// Layout of `bytes`.
    pub encoding: SignatureEncoding,
    /// The encoded signature.
    pub bytes: Vec<u8>,
}
