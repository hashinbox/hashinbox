//! Who may call, without a table or HTTP (design 096): the scope schema
//! ([`HASHINBOX`]). A key's id and secret are `models::user_key`'s. Password hashes,
//! TOTP codes and the recovery mnemonic are `crypto`'s; a session's JWT is
//! `models::session`'s. The tables are the `user`, `user_key` and
//! `user_2fa` models; the checks run in `api::middleware`.

use scope::{Scope, Scopes};

scope::schema! {
    /// Every scope the server checks, a tree: a resource is the first
    /// segment of its routes' URL, a sub-resource sits under its parent
    /// (`networks.endpoints`), and a scope covers its whole subtree. A
    /// wallet's own routes are `wallets[id]` and below. Assets and market
    /// prices need only a session.
    pub(crate) static HASHINBOX = com.hashinbox {
        wallets,
        wallets.indexes,
        wallets.tags,
        wallets.transactions,
        networks,
        networks.endpoints,
        networks.routes,
        networks.transactions,
        triggers,
        triggers.runs,
        jobs,
        logs,
        markets,
        stats,
        users,
        users.keys,
        settings,
        recovery,
    }
}

/// What holding a scope needs beyond its subtree: source → targets
/// (`Scopes::expand`). A target takes the held scope's level (`:read`
/// stays read) unless written `:read`. Published in the OpenAPI document
/// as `x-hashinbox-depends`, so the clients' `can` expands the same.
pub(crate) const DEPENDS: &[(&str, &[&str])] = &[
    // A trigger's send broadcasts from the wallets its selector picks.
    ("com.hashinbox.triggers", &["com.hashinbox.wallets"]),
    // A send is followed as its broadcast job.
    ("com.hashinbox.wallets", &["com.hashinbox.jobs"]),
    // Nodes are added to, and their transactions browsed by, a network:
    // each needs the networks seen.
    (
        "com.hashinbox.networks.endpoints",
        &["com.hashinbox.networks:read"],
    ),
    (
        "com.hashinbox.networks.transactions",
        &["com.hashinbox.networks:read"],
    ),
    // A route picks among the network's nodes.
    (
        "com.hashinbox.networks.routes",
        &["com.hashinbox.networks.endpoints:read"],
    ),
];

/// The first of `scopes` [`HASHINBOX`] does not know (a typo); `None`
/// when it knows each one.
pub(crate) fn first_unknown(scopes: &Scopes) -> Option<&Scope> {
    scopes.iter().find(|scope| !HASHINBOX.knows(scope))
}

/// `scopes` and what they need ([`DEPENDS`]): what a session may do.
pub(crate) fn expanded(scopes: &Scopes) -> Scopes {
    scopes.expand(DEPENDS)
}

/// Every scope: what the root user holds.
pub(crate) fn everything() -> Scopes {
    "com.hashinbox".parse().unwrap_or_default()
}
