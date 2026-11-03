//! Signing against official vectors and round trips.

use crypto::{Ed25519, PrivateKey as _, Secp256k1Ecdsa, Secp256k1Schnorr};
use k256::{
    ecdsa::{RecoveryId, Signature, VerifyingKey, signature::hazmat::PrehashVerifier as _},
    schnorr,
};

fn bytes<const N: usize>(text: &str) -> [u8; N] {
    hex::decode(text)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .unwrap_or_else(|| panic!("bad test hex {text}"))
}

/// BIP-341 `wallet-test-vectors.json`, keyPathSpending inputs 0..3:
/// internal private key, merkle root (empty when none), tweaked private key,
/// sighash.
const KEY_PATH: [[&str; 4]; 3] = [
    [
        "6b973d88838f27366ed61c9ad6367663045cb456e28335c109e30717ae0c6baa",
        "",
        "2405b971772ad26915c8dcdf10f238753a9b837e5f8e6a86fd7c0cce5b7296d9",
        "2514a6272f85cfa0f45eb907fcb0d121b808ed37c6ea160a5a9046ed5526d555",
    ],
    [
        "1e4da49f6aaf4e5cd175fe08a32bb5cb4863d963921255f33d3bc31e1343907f",
        "5b75adecf53548f3ec6ad7d78383bf84cc57b55a3127c72b9a2481752dd88b21",
        "ea260c3b10e60f6de018455cd0278f2f5b7e454be1999572789e6a9565d26080",
        "325a644af47e8a5a2591cda0ab0723978537318f10e6a63d4eed783b96a71a4d",
    ],
    [
        "d3c7af07da2d54f7a7735d3d0fc4f0a73164db638b2f2f7c43f711f6d4aa7e64",
        "c525714a7f49c28aedbbba78c005931a81c234b2f6c99a73e4d06082adc8bf2b",
        "97323385e57015b75b0339a549c56a948eb961555973f0951f555ae6039ef00d",
        "bf013ea93474aa67815b1b6cc441d23b64fa310911d991e713cd34c7f5d46669",
    ],
];

#[test]
fn taproot_tweak_matches_bip341_and_verifies_under_output_key() {
    for [internal, merkle_root, tweaked, sighash] in KEY_PATH {
        let merkle_root = (!merkle_root.is_empty()).then(|| bytes::<32>(merkle_root));
        let key = Secp256k1Schnorr::from_bytes(&bytes::<32>(internal))
            .expect("vector key")
            .tweaked(merkle_root)
            .expect("tweaks");
        let output_key = *schnorr::SigningKey::from_bytes(&bytes::<32>(tweaked))
            .expect("vector tweaked key")
            .verifying_key();
        assert_eq!(
            key.x_only_public_key(),
            <[u8; 32]>::from(output_key.to_bytes())
        );
        let digest = bytes::<32>(sighash);
        let signature = key.sign(&digest).expect("signs");
        let signature = schnorr::Signature::try_from(&signature[..]).expect("64 bytes");
        output_key
            .verify_prehash(&digest, &signature)
            .expect("verifies under the BIP-341 tweaked output key");
    }
}

#[test]
fn untweaked_schnorr_verifies_under_internal_key() {
    let key = Secp256k1Schnorr::generate().expect("randomness");
    let digest = [7_u8; 32];
    let signature = key.sign(&digest).expect("signs");
    let public = schnorr::VerifyingKey::from_bytes(&key.x_only_public_key()).expect("x-only key");
    public
        .verify_prehash(
            &digest,
            &schnorr::Signature::try_from(&signature[..]).expect("64"),
        )
        .expect("verifies");
}

#[test]
fn secp256k1_rejects_non_digest_message() {
    let key = Secp256k1Ecdsa::generate().expect("randomness");
    assert!(key.sign(&[1; 31]).is_err());
}

#[test]
fn ecdsa_round_trip_recovers_signer() {
    let key = Secp256k1Ecdsa::generate().expect("randomness");
    let digest = [9_u8; 32];
    let signed = key.sign(&digest).expect("signs");
    assert_eq!(signed.len(), 65);
    let signature = Signature::from_slice(&signed[..64]).expect("compact");
    let public = VerifyingKey::from_sec1_bytes(&key.compressed_public_key()).expect("sec1");
    public
        .verify_prehash(&digest, &signature)
        .expect("verifies");
    let recovered = VerifyingKey::recover_from_prehash(
        &digest,
        &signature,
        RecoveryId::from_byte(signed[64]).expect("recovery id"),
    )
    .expect("recovers");
    assert_eq!(recovered, public);
}

#[test]
fn ed25519_round_trip_verifies() {
    let key = Ed25519::generate().expect("randomness");
    let message = b"exact message";
    let signed: [u8; 64] = key.sign(message).expect("signs").try_into().expect("64");
    let signature = ed25519_dalek::Signature::from_bytes(&signed);
    let public = ed25519_dalek::VerifyingKey::from_bytes(&key.public_key()).expect("key");
    public.verify_strict(message, &signature).expect("verifies");
    assert!(public.verify_strict(b"other", &signature).is_err());
}

#[test]
fn generated_keys_export_and_reimport() {
    let key = Secp256k1Ecdsa::generate().expect("randomness");
    let again = Secp256k1Ecdsa::from_bytes(key.to_bytes().as_bytes()).expect("imports");
    assert_eq!(key.compressed_public_key(), again.compressed_public_key());
}

#[test]
fn ed25519_keypair_and_base58_import() {
    let key = Ed25519::from_bytes(&[7; 32]).unwrap();
    let mut pair = [7_u8; 64];
    pair[32..].copy_from_slice(&key.public_key());
    let text = bs58::encode(pair).into_string();
    let decoded = crypto::SecretBytes::from_base58(&format!(" {text}\n")).unwrap();
    let imported = Ed25519::from_keypair(decoded.as_bytes()).unwrap();
    assert_eq!(imported.public_key(), key.public_key());

    pair[63] ^= 1;
    assert!(
        Ed25519::from_keypair(&pair).is_err(),
        "a foreign public half"
    );
    assert!(Ed25519::from_keypair(&pair[..32]).is_err(), "not 64 bytes");
    assert!(
        crypto::SecretBytes::from_base58("0OIl").is_err(),
        "not Base58"
    );
}
