use indexing::{BlockHash, IndexScope, SourceError};

use super::error;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceConfig {
    pub scope: IndexScope,
    pub expected_chain_id: u64,
    pub expected_genesis_hash: BlockHash,
}

impl SourceConfig {
    pub fn validate(&self) -> Result<(), SourceError> {
        if self.scope.chain.0 != "ethereum" {
            return Err(self::error(
                "Ethereum index source scope must use the ethereum chain ID",
                false,
            ));
        }
        if self.scope.network.trim().is_empty() {
            return Err(self::error(
                "Ethereum index source network slug must not be empty",
                false,
            ));
        }
        if self.expected_genesis_hash.0.len() != 32 {
            return Err(self::error(
                "configured Ethereum genesis hash must be 32 bytes",
                false,
            ));
        }
        Ok(())
    }
}
