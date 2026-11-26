//! TOTP codes (RFC 6238): HMAC-SHA1, 30-second steps, 6 digits. A code is
//! accepted one step either side of now, and only after the last step
//! used (no replay).

use bitcoin_hashes::{Hash as _, HashEngine as _, Hmac, HmacEngine, sha1};

/// A step's length, seconds.
pub const STEP: i64 = 30;

/// The 6-digit code of `secret` at `step` (RFC 4226's HOTP).
#[must_use]
pub fn code(secret: &[u8], step: i64) -> String {
    let mut engine = HmacEngine::<sha1::Hash>::new(secret);
    engine.input(&step.to_be_bytes());
    let mac = Hmac::<sha1::Hash>::from_engine(engine).to_byte_array();
    let offset = usize::from(mac[19] & 0x0f);
    let bytes = [
        mac[offset],
        mac[offset + 1],
        mac[offset + 2],
        mac[offset + 3],
    ];
    let number = u32::from_be_bytes(bytes) & 0x7fff_ffff;
    format!("{:06}", number % 1_000_000)
}

/// The step `typed` is `secret`'s code at, one step either side of `now`
/// (Unix seconds) and later than `last_step`; `None` when it is none of
/// them.
#[must_use]
pub fn verify(secret: &[u8], typed: &str, now: i64, last_step: Option<i64>) -> Option<i64> {
    let current = now.div_euclid(STEP);
    let mut steps =
        (current - 1..=current + 1).filter(|step| last_step.is_none_or(|last| *step > last));
    steps.find(|step| code(secret, *step) == typed.trim())
}

/// The `otpauth://` URI an authenticator app reads (its QR code) for
/// `secret`, labelled `issuer:account`.
#[must_use]
pub fn uri(secret: &[u8], issuer: &str, account: &str) -> String {
    let (label, issuer) = (escape(&format!("{issuer}:{account}")), escape(issuer));
    format!(
        "otpauth://totp/{label}?secret={}&issuer={issuer}&algorithm=SHA1&digits=6&period={STEP}",
        base32(secret)
    )
}

/// `text` percent-encoded but for RFC 3986's unreserved characters.
fn escape(text: &str) -> String {
    let mut escaped = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            escaped.push(char::from(byte));
        } else {
            escaped.push_str(&format!("%{byte:02X}"));
        }
    }
    escaped
}

/// `bytes` in base 32 (RFC 4648), unpadded: how the URI carries a secret.
fn base32(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut text = String::new();
    let (mut buffer, mut bits) = (0_u32, 0);
    for byte in bytes {
        buffer = ((buffer << 8) | u32::from(*byte)) & 0x1fff;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            text.push(char::from(DIGITS[((buffer >> bits) & 31) as usize]));
        }
    }
    if bits > 0 {
        text.push(char::from(DIGITS[((buffer << (5 - bits)) & 31) as usize]));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{base32, code, uri, verify};

    /// RFC 6238's SHA-1 secret.
    const SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn codes_are_rfc_6238_vectors_last_six_digits() {
        assert_eq!(code(SECRET, 59 / 30), "287082");
        assert_eq!(code(SECRET, 1_111_111_109 / 30), "081804");
        assert_eq!(code(SECRET, 1_234_567_890 / 30), "005924");
        assert_eq!(code(SECRET, 2_000_000_000 / 30), "279037");
    }

    #[test]
    fn a_code_is_accepted_a_step_either_side_once() {
        let now = 1_111_111_109;
        let step = now / 30;
        assert_eq!(verify(SECRET, "081804", now, None), Some(step));
        assert_eq!(verify(SECRET, "081804", now + 30, None), Some(step));
        assert_eq!(verify(SECRET, "081804", now - 30, None), Some(step));
        assert_eq!(verify(SECRET, "081804", now + 60, None), None);
        assert_eq!(verify(SECRET, "081804", now, Some(step)), None, "replayed");
        assert_eq!(verify(SECRET, "000000", now, None), None);
    }

    #[test]
    fn the_uri_names_the_account_and_the_base32_secret() {
        assert_eq!(
            uri(b"foobar", "HashInBox", "a@b.c"),
            "otpauth://totp/HashInBox%3Aa%40b.c?secret=MZXW6YTBOI&issuer=HashInBox\
             &algorithm=SHA1&digits=6&period=30"
        );
    }

    #[test]
    fn base32_is_rfc_4648() {
        assert_eq!(base32(b""), "");
        assert_eq!(base32(b"f"), "MY");
        assert_eq!(base32(b"fo"), "MZXQ");
        assert_eq!(base32(b"foo"), "MZXW6");
        assert_eq!(base32(b"foob"), "MZXW6YQ");
        assert_eq!(base32(b"fooba"), "MZXW6YTB");
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
    }
}
