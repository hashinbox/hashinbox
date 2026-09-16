use indexing::{BlockHash, SourceError};

use crate::Network;

use super::source_error;

/// Strict Bitcoin Core identity and readiness requirements shared by wallets and indexers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreConfig {
    pub expected_network: Network,
    pub expected_genesis_hash: Option<BlockHash>,
}

impl CoreConfig {
    /// Verifies the requested Bitcoin network without pinning a specific genesis hash.
    #[must_use]
    pub const fn new(expected_network: Network) -> Self {
        Self {
            expected_network,
            expected_genesis_hash: None,
        }
    }

    /// Pins the expected genesis block using Bitcoin's displayed hash byte order.
    pub fn with_genesis_hash(mut self, genesis_hash: &str) -> Result<Self, SourceError> {
        self.expected_genesis_hash = Some(super::parse_bitcoin_block_hash(genesis_hash)?);
        Ok(self)
    }

    pub fn validate(&self) -> Result<(), SourceError> {
        if self
            .expected_genesis_hash
            .as_ref()
            .is_some_and(|hash| hash.0.len() != 32)
        {
            return Err(source_error(
                "configured Bitcoin genesis hash must be 32 bytes",
                false,
            ));
        }
        Ok(())
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
    fn constructor_preserves_bitcoin_hash_byte_order_and_parse_errors() {
        let genesis = "0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206";
        let config = CoreConfig::new(Network::Regtest)
            .with_genesis_hash(genesis)
            .unwrap();
        assert_eq!(config.expected_network, Network::Regtest);
        assert_eq!(
            crate::format_bitcoin_block_hash(config.expected_genesis_hash.as_ref().unwrap())
                .unwrap(),
            genesis
        );
        assert_eq!(config.expected_genesis_hash.as_ref().unwrap().0[0], 0x06);
        config.validate().unwrap();
        for invalid in ["", "abc", "not-a-hash"] {
            assert_eq!(
                CoreConfig::new(Network::Regtest)
                    .with_genesis_hash(invalid)
                    .unwrap_err(),
                crate::parse_bitcoin_block_hash(invalid).unwrap_err()
            );
        }
    }

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
