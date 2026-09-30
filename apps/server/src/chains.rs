//! The composition root: the only module that names a chain crate
//! (`scripts/check-chains.sh` check 8).
//!
//! Every chain knows how to assemble itself, `sdk/config` knows how to hand a
//! document's named section to the chain that registered for it, and
//! `wallets::Composition` knows how to hold any number of them: a chain
//! crate's own `Config<AssetTag>` carries its section, `connect` verifies its
//! node's identity without opening anything, and the verified node is a
//! `wallets::Connection` that composes over this process's repository.
//!
//! What is left here is three registration lines. A fourth chain is a fourth
//! line; nothing else in this binary would change.

use sdk_config::Registry;

use crate::{api::AssetTag, storage::Repository};

/// Which chain owns which section of `chains` in the configuration document.
///
/// The names are the document's, stated once, here. Every arm this used to
/// have — an `Option` field per chain, a `validate` branch, a `networks`
/// branch, a `connect` branch, and a `println!` that had to name Bitcoin from
/// outside to print its confirmation depth — is now the chain's own
/// `impl sdk_config::Chain`.
pub(crate) fn registry() -> Registry<AssetTag, Repository> {
    let mut registry = Registry::new();
    registry.register::<chain_bitcoin::Config<AssetTag>>("bitcoin");
    registry.register::<chain_ethereum::Config<AssetTag>>("ethereum");
    registry.register::<chain_solana::Config<AssetTag>>("solana");
    registry
}
