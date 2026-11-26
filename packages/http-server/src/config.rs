use std::{error::Error, fmt, net::SocketAddr};

use crate::BearerToken;

/// Where TLS happens, which decides the addresses a listener may bind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportSecurity {
    /// Plain HTTP is permitted only when the listener is bound to loopback.
    PlaintextLoopback,
    /// A trusted upstream proxy is responsible for TLS termination.
    TlsTerminatedUpstream,
}

/// Per-request bounds applied to protected routes; default body limit 1 MiB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestLimits {
    max_body_bytes: usize,
}

impl RequestLimits {
    /// Sets the maximum request body size; zero is rejected.
    pub fn new(max_body_bytes: usize) -> Result<Self, ConfigError> {
        if max_body_bytes == 0 {
            return Err(ConfigError::new(
                ConfigErrorKind::InvalidLimits,
                "maximum request body size must be greater than zero",
            ));
        }
        Ok(Self { max_body_bytes })
    }

    /// Maximum request body size in bytes.
    #[must_use]
    pub const fn max_body_bytes(&self) -> usize {
        self.max_body_bytes
    }
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            max_body_bytes: 1024 * 1024,
        }
    }
}

/// Listener and middleware policy, checked by [`Config::validate`].
#[derive(Clone, Debug)]
pub struct Config {
    pub(crate) bind_addr: SocketAddr,
    transport_security: TransportSecurity,
    pub(crate) bearer_token: Option<BearerToken>,
    pub(crate) limits: RequestLimits,
}

impl Config {
    /// Builds a configuration; call [`Config::validate`] (or any router
    /// builder) to check it.
    #[must_use]
    pub const fn new(
        bind_addr: SocketAddr,
        transport_security: TransportSecurity,
        bearer_token: Option<BearerToken>,
        limits: RequestLimits,
    ) -> Self {
        Self {
            bind_addr,
            transport_security,
            bearer_token,
            limits,
        }
    }

    /// Rejects a non-loopback bind without upstream TLS. The bearer token is
    /// optional: an application may authenticate each request itself.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.bind_addr.ip().is_loopback()
            && self.transport_security != TransportSecurity::TlsTerminatedUpstream
        {
            return Err(ConfigError::new(
                ConfigErrorKind::InsecureNonLoopbackBind,
                "a non-loopback listener requires upstream TLS termination",
            ));
        }
        Ok(())
    }
}

/// Which configuration rule a [`ConfigError`] broke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigErrorKind {
    /// A [`RequestLimits`] bound is zero.
    InvalidLimits,
    /// The [`BearerToken`] is empty or contains whitespace or control characters.
    InvalidBearerToken,
    /// A non-loopback bind without [`TransportSecurity::TlsTerminatedUpstream`].
    InsecureNonLoopbackBind,
}

/// An unsafe or invalid server configuration, rejected before serving.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    /// The broken rule.
    pub kind: ConfigErrorKind,
    /// Human-readable context; never contains the token.
    pub message: String,
}

impl ConfigError {
    pub(crate) fn new(kind: ConfigErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(bind: &str, security: TransportSecurity) -> Config {
        Config::new(
            bind.parse().unwrap(),
            security,
            None,
            RequestLimits::default(),
        )
    }

    #[test]
    fn non_loopback_needs_upstream_tls_not_a_token() {
        let plaintext = config("0.0.0.0:8080", TransportSecurity::PlaintextLoopback);
        assert_eq!(
            plaintext.validate().unwrap_err().kind,
            ConfigErrorKind::InsecureNonLoopbackBind
        );
        let upstream = config("0.0.0.0:8080", TransportSecurity::TlsTerminatedUpstream);
        assert_eq!(upstream.validate(), Ok(()));
        let loopback = config("127.0.0.1:8080", TransportSecurity::PlaintextLoopback);
        assert_eq!(loopback.validate(), Ok(()));
    }
}
