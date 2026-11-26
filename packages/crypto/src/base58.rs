//! Base 58 (Bitcoin's alphabet, no check bytes): text with no `0`, `O`,
//! `I` or `l`, and nothing a URL or a double-click splits.

/// `bytes` in base 58; each leading zero byte is a leading `1`.
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    bs58::encode(bytes).into_string()
}

/// The bytes base-58 `text` is; `None` for a character not in the
/// alphabet.
#[must_use]
pub fn decode(text: &str) -> Option<Vec<u8>> {
    bs58::decode(text).into_vec().ok()
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};

    #[test]
    fn it_round_trips_and_refuses_a_foreign_character() {
        assert_eq!(encode(b"hello world"), "StV1DL6CwTryKyV");
        assert_eq!(encode(&[0, 0, 1]), "112");
        assert_eq!(
            decode("StV1DL6CwTryKyV").as_deref(),
            Some(&b"hello world"[..])
        );
        assert_eq!(decode("0OIl"), None);
    }
}
