//! Test support (feature `testing`): the [`fixtures`] the crate's tests
//! share.

pub mod fixtures {
    //! A localnet with one test mint.

    use crate::{Config, Network, Token};

    /// The test mint (mainnet USDC's address).
    pub const MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    /// `length` bytes of `byte`, base58.
    #[must_use]
    pub fn base58(byte: u8, length: usize) -> String {
        bs58::encode(vec![byte; length]).into_string()
    }

    /// A localnet whose genesis hash is 32 bytes of 9, with [`MINT`] at 6
    /// decimals.
    ///
    /// # Panics
    /// Never: the hash and [`MINT`] parse.
    #[must_use]
    pub fn config() -> Config {
        let genesis_hash = base58(9, 32).parse().expect("a hash");
        let mut config = Config::new(Network::new(genesis_hash, "localnet"));
        config.tokens = vec![Token {
            mint: MINT.parse().expect("an address"),
            decimals: 6,
            ticker: None,
            display_decimals: None,
        }];
        config
    }

    /// [`config`]'s network, as base names it.
    #[must_use]
    pub fn network() -> base::Network {
        base::Network::from(&config().native())
    }
}
