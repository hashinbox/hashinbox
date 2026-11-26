//! [`Network`]: a Tron network's identity, and the native currency.

use anyhow::ensure;
use base::Chain;

/// The native currency's name.
pub const TRX_NAME: &str = "Tron";
/// The native currency's ticker.
pub const TRX_TICKER: &str = "TRX";
/// Sun digits in one TRX.
pub const TRX_DECIMALS: u32 = 6;

/// The chain family name.
pub const CHAIN_NAME: Chain = Chain::from_static("tron");

/// A Tron network: the id of its genesis block, which every node must
/// report, and its stable application name. Neither is derived from the
/// other: a private network has its own genesis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    /// The genesis block's id: 64 lowercase hex digits (mainnet
    /// `00000000000000001ebf…6653dc`).
    pub genesis: String,
    /// Stable application name of the network, such as `mainnet` or `nile`.
    pub slug: String,
}

impl Network {
    /// The network of `genesis` named `slug`.
    #[must_use]
    pub fn new(genesis: impl Into<String>, slug: impl Into<String>) -> Self {
        Self {
            genesis: genesis.into(),
            slug: slug.into(),
        }
    }

    /// Rejects a genesis id that is not 64 lowercase hex digits, or a blank
    /// slug.
    ///
    /// # Errors
    ///
    /// When either is malformed.
    pub fn validate(&self) -> anyhow::Result<()> {
        let hex = |c: char| c.is_ascii_digit() || ('a'..='f').contains(&c);
        ensure!(
            self.genesis.len() == 64 && self.genesis.chars().all(hex),
            "Tron genesis id must be 64 lowercase hex digits"
        );
        ensure!(
            !self.slug.trim().is_empty(),
            "Tron network needs a non-empty slug"
        );
        Ok(())
    }
}

/// The chain-neutral identity of this network: `(tron, slug)`.
impl From<&Network> for base::Network {
    fn from(network: &Network) -> Self {
        Self::new(CHAIN_NAME, network.slug.clone())
    }
}
