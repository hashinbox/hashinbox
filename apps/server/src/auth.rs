//! Who may call, without a table or HTTP (design 096): the scope schema
//! ([`HASHINBOX`]). A key's id and secret are `models::user_key`'s. Password hashes,
//! TOTP codes and the recovery mnemonic are `crypto`'s; a session's JWT is
//! `models::session`'s. The tables are the `user`, `user_key` and
//! `user_2fa` models; the checks run in `api::middleware`.

use scope::{Scope, Scopes};

scope::schema! {
    /// Every scope the server checks: each resource is the first segment of
    /// its routes' URL (`endpoints`, which holds node URLs and their API
    /// keys, apart from `networks`). A wallet's own routes (indexes, legs,
    /// tags, sends) are `wallets[id]`, read or write.
    pub(crate) static HASHINBOX = com.hashinbox {
        wallets => [read, write],
        transactions => [read],
        networks => [read],
        endpoints => [read, write],
        triggers => [read, write],
        jobs => [read],
        logs => [read],
        markets => [read, write],
        assets => [read],
        stats => [read],
        keys => [read, write],
        users => [read, write],
        settings => [read, write],
        recovery => [write],
    }
}

/// The first of `scopes` [`HASHINBOX`] does not know (a typo); `None`
/// when it knows each one.
pub(crate) fn first_unknown(scopes: &Scopes) -> Option<&Scope> {
    scopes.iter().find(|scope| !HASHINBOX.knows(scope))
}

/// Every scope: what the root user holds.
pub(crate) fn everything() -> Scopes {
    Scopes::from_iter([scope::scope!(com.hashinbox.* => *)])
}
