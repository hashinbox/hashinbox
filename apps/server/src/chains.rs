//! The composition root's chain list: the only module that names a chain
//! crate. Each chain's `Config` reads its own section of the payments
//! document; a fourth chain is a fourth line.

use payments::Builder;

/// `builder` with every chain registered under its section name.
pub(crate) fn register(builder: Builder) -> Builder {
    builder
        .with_chain::<chain_bitcoin::Config>("bitcoin")
        .with_chain::<chain_ethereum::Config>("ethereum")
        .with_chain::<chain_solana::Config>("solana")
}
