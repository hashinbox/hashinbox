//! What the crate's tests share: mainnet with one test token.

use base::Decimal;
use num_bigint::BigUint;

use crate::{Config, Network, Token};

/// The test token's contract.
pub const TOKEN: &str = "0x00000000000000000000000000000000000000c0";

/// Mainnet, as base names it.
#[must_use]
pub fn network() -> base::Network {
    base::Network::new(crate::CHAIN_NAME, "mainnet")
}

/// `amount` atomic units.
#[must_use]
pub fn atomic(amount: u64) -> Decimal {
    Decimal::from_atomic(BigUint::from(amount), 0)
}

/// Mainnet with [`TOKEN`] at 6 decimals.
///
/// # Panics
/// Never: [`TOKEN`] parses.
#[must_use]
pub fn config() -> Config {
    let mut config = Config::new(Network::new(1, "mainnet"));
    config.tokens = vec![Token {
        contract: TOKEN.parse().expect("an address"),
        decimals: 6,
        ticker: None,
        display_decimals: None,
    }];
    config
}
