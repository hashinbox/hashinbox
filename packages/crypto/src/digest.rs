//! Checksums and hashes of bytes: SHA-256 and CRC32.

use sha2::Digest as _;

/// The SHA-256 of `bytes`.
#[must_use]
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    sha2::Sha256::digest(bytes).into()
}

/// The CRC32 (ISO-HDLC, as zip and PNG) of `bytes`: a typo check, not a
/// hash.
#[must_use]
pub fn crc32(bytes: &[u8]) -> u32 {
    crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC).checksum(bytes)
}

#[cfg(test)]
mod tests {
    use super::{crc32, sha256};

    #[test]
    fn crc32_and_sha256_vectors() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(
            hex::encode(sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
