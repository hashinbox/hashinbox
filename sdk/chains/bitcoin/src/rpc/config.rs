use crate::Network;

/// Strict Bitcoin Core identity and readiness requirements shared by wallets and indexers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreConfig {
    pub expected_network: Network,
}

impl CoreConfig {
    /// Verifies the requested Bitcoin network.
    #[must_use]
    pub const fn new(expected_network: Network) -> Self {
        Self { expected_network }
    }
}

/// Bitcoin Core RPC credentials, encoded for the caller's HTTP transport configuration.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub user: String,
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Credentials")
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// Creates an HTTP Basic authorization header for `json_rpc::Config::headers`.
    #[must_use]
    pub fn header(&self) -> (String, String) {
        use base64::Engine;

        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.user, self.password));
        ("Authorization".to_owned(), format!("Basic {encoded}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_encode_basic_auth_and_redact_debug() {
        use base64::Engine;

        let credentials = Credentials {
            user: "rpc-user".to_owned(),
            password: "rpc-password:extra".to_owned(),
        };
        let (name, value) = credentials.header();
        assert_eq!(name, "Authorization");
        let encoded = value.strip_prefix("Basic ").unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        assert_eq!(decoded, b"rpc-user:rpc-password:extra");
        let debug = format!("{credentials:?}");
        assert!(!debug.contains(&credentials.password));
        assert!(!debug.contains(&credentials.user));
        assert!(!debug.contains(encoded));
    }
}
