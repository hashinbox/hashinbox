use std::{fmt, sync::Arc};

use axum::http::HeaderValue;

use crate::{ConfigError, ConfigErrorKind};

/// The shared secret of bearer authentication; `Debug` is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct BearerToken(Arc<[u8]>);

impl BearerToken {
    /// Accepts a non-empty token without whitespace or control characters.
    pub fn new(token: impl AsRef<str>) -> Result<Self, ConfigError> {
        let bytes = token.as_ref().as_bytes();
        if bytes.is_empty()
            || bytes
                .iter()
                .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        {
            return Err(ConfigError::new(
                ConfigErrorKind::InvalidBearerToken,
                "bearer token must be non-empty and contain no whitespace or control characters",
            ));
        }
        Ok(Self(Arc::from(bytes)))
    }

    /// Compares equal-length credentials using `constant_time_eq`.
    /// Missing headers, invalid prefixes, and unequal lengths may return early;
    /// credential length is not hidden.
    #[must_use]
    pub fn matches_authorization_header(&self, value: Option<&HeaderValue>) -> bool {
        const PREFIX: &[u8] = b"Bearer ";
        let Some(value) = value else {
            return false;
        };
        let bytes = value.as_bytes();
        if bytes.len() < PREFIX.len() || !bytes.starts_with(PREFIX) {
            return false;
        }
        constant_time_eq::constant_time_eq(&bytes[PREFIX.len()..], &self.0)
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerToken([REDACTED])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_requires_every_credential_byte_to_match() {
        let token = BearerToken::new("correct-secret").expect("fixture token is valid");
        assert!(
            token.matches_authorization_header(Some(&HeaderValue::from_static(
                "Bearer correct-secret",
            )))
        );

        for value in [
            "Bearer xorrect-secret",
            "Bearer correctxsecret",
            "Bearer correct-secrex",
        ] {
            assert!(!token.matches_authorization_header(Some(&HeaderValue::from_static(value))));
        }
    }

    #[test]
    fn authorization_rejects_shorter_longer_and_empty_credentials() {
        let token = BearerToken::new("correct-secret").expect("fixture token is valid");
        for value in [
            "Bearer correct-secre",
            "Bearer correct-secret-extra",
            "Bearer ",
        ] {
            assert!(!token.matches_authorization_header(Some(&HeaderValue::from_static(value))));
        }
    }

    #[test]
    fn authorization_requires_the_exact_bearer_prefix_without_trimming() {
        let token = BearerToken::new("correct-secret").expect("fixture token is valid");
        assert!(!token.matches_authorization_header(None));
        for value in [
            "",
            "Bearer",
            "correct-secret",
            "Basic correct-secret",
            "bearer correct-secret",
            "Bearer  correct-secret",
            "Bearer correct-secret ",
        ] {
            assert!(!token.matches_authorization_header(Some(&HeaderValue::from_static(value))));
        }
    }
}
