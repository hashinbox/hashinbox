//! The composition root: the only module that names a chain crate
//! (`scripts/check-chains.sh` check 8).
//!
//! Every chain knows how to assemble itself, `payments::config` knows how to hand a
//! document's named section to the chain that registered for it, and
//! `payments::config::Composition` knows how to hold any number of them: a chain
//! crate's own `Config<AssetTag>` carries its section, `connect` verifies its
//! node's identity without opening anything, and the verified node is a
//! `payments::config::Connection` that composes over the container's store.
//!
//! What is left here is three registration lines. A fourth chain is a fourth
//! line; nothing else in this binary would change.

use payments::Builder;

use crate::asset::AssetTag;

/// `builder` with the chain that owns each section of `chains` in the
/// configuration document registered.
///
/// The names are the document's, stated once, here.
pub(crate) fn register(builder: Builder<AssetTag>) -> Builder<AssetTag> {
    builder
        .with::<chain_bitcoin::Config<AssetTag>>("bitcoin")
        .with::<chain_ethereum::Config<AssetTag>>("ethereum")
        .with::<chain_solana::Config<AssetTag>>("solana")
}
