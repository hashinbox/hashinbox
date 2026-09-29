use crate::{
    PublicKey, PublicKeyFormat, ScalarTweak, Signature, SignatureEncoding, SignatureScheme,
};

/// Bytes to sign as-is.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Digest {
    /// The chain or protocol computes this digest. The signer does not hash it again.
    pub bytes: Vec<u8>,
}

/// What a signer signs.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SignablePayload {
    /// A whole message the scheme hashes itself (Ed25519 signs messages).
    Message(Vec<u8>),
    /// A digest the chain already computed (secp256k1 signs digests).
    Digest(Digest),
}

/// Everything a caller asks of a signer: payload, scheme, and output shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignRequest {
    /// Message or precomputed digest.
    pub payload: SignablePayload,
    /// Signature algorithm.
    pub scheme: SignatureScheme,
    /// Output encoding, e.g. recoverable ECDSA or DER.
    pub encoding: SignatureEncoding,
    /// Shape of the returned public key.
    pub public_key_format: PublicKeyFormat,
    /// Key tweak applied before signing (Schnorr/Taproot only).
    pub key_tweak: Option<ScalarTweak>,
}

/// A signature with the public key that verifies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedPayload {
    /// The signature in the requested scheme and encoding.
    pub signature: Signature,
    /// The signer's untweaked public key in the requested format.
    pub public_key: PublicKey,
}
