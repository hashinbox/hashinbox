//! The codes (`target`s) of the events the server logs, and the one catalog
//! of every event's message (`logs.json`, `{ code: template }`), the other
//! crates' (`rpc::events`, `tasks::events`, `base::events`) too. A template
//! is filled from the event's fields and its spans' (`{key}`).

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};

/// A new database file created: its schema and starting rows applied.
pub(crate) const APP_DB_CREATED: &str = "app.db.created";
/// The root user and settings created from `--config` on an empty
/// database.
pub(crate) const APP_BOOTSTRAP: &str = "app.bootstrap";
/// The server started.
pub(crate) const APP_START: &str = "app.start";
/// The server stopped.
pub(crate) const APP_STOP: &str = "app.stop";
/// A payment queued.
pub(crate) const SEND_QUEUED: &str = "send.queued";
/// A payment signed.
pub(crate) const SEND_SIGN_SIGNED: &str = "send.sign.signed";
/// A payment not signed; retried.
pub(crate) const SEND_SIGN_RETRIED: &str = "send.sign.retried";
/// A payment not signed; failed for good.
pub(crate) const SEND_SIGN_FAILED: &str = "send.sign.failed";
/// A payment broadcast: accepted or already mined.
pub(crate) const SEND_BROADCAST_ACCEPTED: &str = "send.broadcast.accepted";
/// A payment broadcast: refused, or forgotten.
pub(crate) const SEND_BROADCAST_REFUSED: &str = "send.broadcast.refused";
/// A payment broadcast: no node said whether it took it.
pub(crate) const SEND_BROADCAST_UNSURE: &str = "send.broadcast.unsure";
/// A payment's cancel sent.
pub(crate) const SEND_CANCEL: &str = "send.cancel";
/// A payment bumped.
pub(crate) const SEND_BUMP: &str = "send.bump";
/// A payment final.
pub(crate) const SEND_END_FINAL: &str = "send.end.final";
/// A payment refused or cancelled.
pub(crate) const SEND_END_STOPPED: &str = "send.end.stopped";
/// A payment failed for good.
pub(crate) const SEND_END_FAILED: &str = "send.end.failed";
/// A node enabled.
pub(crate) const ENDPOINT_ENABLED: &str = "endpoint.enabled";
/// A node disabled.
pub(crate) const ENDPOINT_DISABLED: &str = "endpoint.disabled";
/// A node's disable not stored.
pub(crate) const ENDPOINT_DISABLE_FAILED: &str = "endpoint.disable_failed";
/// A node's call not counted.
pub(crate) const ENDPOINT_UNCOUNTED: &str = "endpoint.uncounted";
/// A network has an enabled node again.
pub(crate) const NETWORK_SERVED: &str = "network.served";
/// A network has no enabled node.
pub(crate) const NETWORK_UNSERVED: &str = "network.unserved";
/// A node demoted.
pub(crate) const NODE_DEMOTED: &str = "node.demoted";
/// A node restored.
pub(crate) const NODE_RESTORED: &str = "node.restored";
/// A call moved on to the next node.
pub(crate) const RPC_FAILOVER: &str = "rpc.failover";
/// A block kept, some of it matched.
pub(crate) const INDEX_BLOCK_MATCHED: &str = "index.block.matched";
/// A block kept, nothing of it matched.
pub(crate) const INDEX_BLOCK_UNMATCHED: &str = "index.block.unmatched";
/// A wallet leg kept.
pub(crate) const INDEX_LEG: &str = "index.leg";
/// A watched transaction found.
pub(crate) const INDEX_FOUND: &str = "index.found";
/// A wallet index settled: the chain's balance is the kept legs' total.
pub(crate) const INDEX_SETTLE_UNCHANGED: &str = "index.settle.unchanged";
/// A wallet index settled: the history has a gap, the balance adjusted.
pub(crate) const INDEX_SETTLE_ADJUSTED: &str = "index.settle.adjusted";
/// A trigger refused.
pub(crate) const TRIGGER_REFUSED: &str = "trigger.refused";
/// A trigger skipped.
pub(crate) const TRIGGER_SKIPPED: &str = "trigger.skipped";
/// A trigger queued a payment.
pub(crate) const TRIGGER_FIRED: &str = "trigger.fired";
/// A trigger round in which some trigger fired or was refused.
pub(crate) const TRIGGER_ROUND_ACTED: &str = "trigger.round.acted";
/// A trigger round in which none did.
pub(crate) const TRIGGER_ROUND_IDLE: &str = "trigger.round.idle";

/// A price feed's request answered, or not (debug).
pub(crate) const MARKET_FEED_ANSWERED: &str = "market.feed.answered";
/// A market's price feed turned on or off.
pub(crate) const MARKET_FEED_SET: &str = "market.feed.set";
/// A price feed failed: the first of a streak.
pub(crate) const MARKET_FEED_FAILED: &str = "market.feed.failed";
/// A price feed answered again after failing.
pub(crate) const MARKET_FEED_RECOVERED: &str = "market.feed.recovered";

/// A login answered with a session: its `method`, `pwd` or `passphrase`.
pub(crate) const AUTH_LOGIN_OK: &str = "auth.login.ok";
/// A login refused: a wrong email, password, passphrase or code (which,
/// never said); its `method`.
pub(crate) const AUTH_LOGIN_FAILED: &str = "auth.login.failed";
/// A user created (the root one too).
pub(crate) const AUTH_USER_CREATED: &str = "auth.user.created";
/// A key created.
pub(crate) const AUTH_KEY_CREATED: &str = "auth.key.created";
/// A key revoked.
pub(crate) const AUTH_KEY_REVOKED: &str = "auth.key.revoked";
/// A user's two-factor turned on (its first code confirmed).
pub(crate) const AUTH_2FA_ON: &str = "auth.2fa.on";
/// A user's two-factor turned off.
pub(crate) const AUTH_2FA_OFF: &str = "auth.2fa.off";
/// The recovery passphrase created where none was set (never its words).
pub(crate) const AUTH_RECOVERY_CREATED: &str = "auth.recovery.created";
/// The recovery passphrase replaced by new words (never either).
pub(crate) const AUTH_RECOVERY_REPLACED: &str = "auth.recovery.replaced";
/// The recovery passphrase removed: passphrase logins are off.
pub(crate) const AUTH_RECOVERY_REMOVED: &str = "auth.recovery.removed";
/// A request refused for want of a scope.
pub(crate) const AUTH_DENIED: &str = "auth.denied";
/// A setting written: its key, and its value when public.
pub(crate) const SETTING_CHANGED: &str = "setting.changed";

/// The catalog, `{ code: template }`.
pub(crate) static MESSAGES: LazyLock<BTreeMap<String, String>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../logs.json")).expect("logs.json is a map of text")
});

/// `template` with each `{key}` of `fields` filled, a missing one left as
/// `{key}`; and the keys it filled.
pub(crate) fn fill<'a>(
    template: &str,
    fields: &'a BTreeMap<String, String>,
) -> (String, BTreeSet<&'a str>) {
    let (mut text, mut used, mut rest) = (String::new(), BTreeSet::new(), template);
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}').map(|close| open + close) else {
            break;
        };
        text.push_str(&rest[..open]);
        let key = &rest[open + 1..close];
        match fields.get_key_value(key) {
            Some((key, value)) => {
                text.push_str(value);
                used.insert(key.as_str());
            }
            None => text.push_str(&rest[open..=close]),
        }
        rest = &rest[close + 1..];
    }
    text.push_str(rest);
    (text, used)
}
