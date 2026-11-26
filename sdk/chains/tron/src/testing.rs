//! Test support (feature `testing`): the [`fixtures`] the crate's tests
//! share.

pub mod fixtures {
    //! Nile with its test USDT.

    use crate::{Config, Network, Token};

    /// Nile's genesis block id.
    pub const NILE: &str = "0000000000000000d698d4192c56cb6be724a558448e2684802de4d6cd8690dc";

    /// Nile's test USDT (6 decimals).
    pub const USDT: &str = "TXYZopYRdj2D9XRtbG411XZZ3kM5VkAeBf";

    /// Nile, with [`USDT`] at 6 decimals.
    ///
    /// # Panics
    /// Never: [`USDT`] parses.
    #[must_use]
    pub fn config() -> Config {
        let mut config = Config::new(Network::new(NILE, "nile"));
        config.tokens = vec![Token {
            contract: USDT.parse().expect("an address"),
            decimals: 6,
            ticker: Some("USDT".to_owned()),
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
