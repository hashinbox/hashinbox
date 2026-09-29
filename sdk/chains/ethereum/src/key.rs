//! The SDK signing contract for the native [`KeyPair`].

use base::{SignFuture, SignRequest, Signer};

use crate::native::KeyPair;

impl Signer for KeyPair {
    fn sign<'a>(&'a self, request: SignRequest) -> SignFuture<'a> {
        self.pair.sign(request)
    }
}

#[cfg(test)]
mod tests {
    use base::{Digest, PublicKeyFormat, SignablePayload, SignatureEncoding, SignatureScheme};
    use futures_executor::block_on;

    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn a_fixed_secret_and_digest_sign_to_fixed_bytes() {
        let key = KeyPair::import(base::SecretBytes::new([1; 32])).unwrap();
        let signed = block_on(key.sign(SignRequest {
            payload: SignablePayload::Digest(Digest {
                bytes: vec![0x42; 32],
            }),
            scheme: SignatureScheme::EcdsaSecp256k1,
            encoding: SignatureEncoding::Recoverable,
            public_key_format: PublicKeyFormat::Raw,
            key_tweak: None,
        }))
        .unwrap();

        assert_eq!(
            hex(&signed.signature.bytes),
            "351f7775548e4c561dd9726a9ce477c78f6854ab2db3815e69792463fa54f1fa\
             46192778740b115adf0529c6d1730b22d5a0636ff8a0d3ea6d7e021a144c0f0300"
        );
        assert_eq!(
            hex(&signed.public_key.bytes),
            "1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f\
             70beaf8f588b541507fed6a642c5ab42dfdf8120a7f639de5122d47a69a8e8d1"
        );
    }

    #[test]
    fn generation_signs_for_its_address() {
        let key = KeyPair::generate().unwrap();
        let digest = [0x42; 32];
        let signed = block_on(key.sign(SignRequest {
            payload: SignablePayload::Digest(Digest {
                bytes: digest.to_vec(),
            }),
            scheme: SignatureScheme::EcdsaSecp256k1,
            encoding: SignatureEncoding::Recoverable,
            public_key_format: PublicKeyFormat::Raw,
            key_tweak: None,
        }))
        .unwrap();
        let signature =
            alloy_primitives::Signature::try_from(signed.signature.bytes.as_slice()).unwrap();
        let recovered = signature
            .recover_address_from_prehash(&digest.into())
            .unwrap();
        assert_eq!(key.address().as_bytes(), &recovered.0.0);
        assert_eq!(signed.public_key.format, PublicKeyFormat::Raw);
        assert_eq!(signed.public_key.bytes.len(), 64);
    }
}
