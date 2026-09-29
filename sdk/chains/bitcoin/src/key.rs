//! The SDK signing contract for the native [`KeyPair`].
//!
//! A Bitcoin key names no network, so it has no address of its own: the
//! wallet it is attached to derives one from its configuration.

use base::{SignFuture, SignRequest, Signer};

use crate::native::KeyPair;

impl Signer for KeyPair {
    fn sign<'a>(&'a self, request: SignRequest) -> SignFuture<'a> {
        self.pair.sign(request)
    }
}

#[cfg(test)]
mod tests {
    use base::{
        Digest, PublicKeyFormat, SecretBytes, SignablePayload, SignatureEncoding, SignatureScheme,
    };
    use bitcoin::{
        CompressedPublicKey,
        secp256k1::{Message, Secp256k1, ecdsa},
    };
    use futures_executor::block_on;

    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn request(
        scheme: SignatureScheme,
        encoding: SignatureEncoding,
        public_key_format: PublicKeyFormat,
        key_tweak: Option<base::ScalarTweak>,
    ) -> SignRequest {
        SignRequest {
            payload: SignablePayload::Digest(Digest { bytes: vec![9; 32] }),
            scheme,
            encoding,
            public_key_format,
            key_tweak,
        }
    }

    #[test]
    fn a_fixed_secret_and_digest_sign_to_fixed_bytes_on_both_schemes() {
        let key = KeyPair::import(SecretBytes::new([1; 32])).unwrap();
        let ecdsa = block_on(key.sign(request(
            SignatureScheme::EcdsaSecp256k1,
            SignatureEncoding::Der,
            PublicKeyFormat::Compressed,
            None,
        )))
        .unwrap();
        let schnorr = block_on(key.sign(request(
            SignatureScheme::SchnorrSecp256k1,
            SignatureEncoding::Raw,
            PublicKeyFormat::XOnly,
            Some(base::ScalarTweak::TaggedHashAdd {
                tag: b"TapTweak".to_vec(),
                suffix: Vec::new(),
            }),
        )))
        .unwrap();

        assert_eq!(
            hex(&ecdsa.signature.bytes),
            "30450221008f3e7e4af52c3c7fc2e1705fbeed56332f02d716a02840043cf2221d7f5a2ce9\
             02203789baacdf01aa93e2d8031ebaf266db82aa3bbef168b004e804041ba844da45"
        );
        assert_eq!(
            hex(&ecdsa.public_key.bytes),
            "031b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f"
        );
        assert_eq!(
            hex(&schnorr.signature.bytes),
            "d8a901785eeb08c9710f77fe7ee965b64653e6c25276ebe932485dbbb3ce5623\
             c2806b4a8491ee983da2f1338fb72587123a6bf77f77f32b64aa62acf0aafc15"
        );
        assert_eq!(
            hex(&schnorr.public_key.bytes),
            "1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f"
        );
    }

    #[test]
    fn signatures_verify_under_the_key_every_address_derives_from() {
        let key = KeyPair::import(SecretBytes::new([1; 32])).unwrap();
        let signed = block_on(key.sign(SignRequest {
            payload: SignablePayload::Digest(Digest { bytes: vec![9; 32] }),
            scheme: SignatureScheme::EcdsaSecp256k1,
            encoding: SignatureEncoding::Der,
            public_key_format: PublicKeyFormat::Compressed,
            key_tweak: None,
        }))
        .unwrap();
        let public = CompressedPublicKey::from_slice(&signed.public_key.bytes).unwrap();
        let signature = ecdsa::Signature::from_der(&signed.signature.bytes).unwrap();
        Secp256k1::verification_only()
            .verify_ecdsa(&Message::from_digest([9; 32]), &signature, &public.0)
            .unwrap();
    }
}
