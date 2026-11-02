//! Sending: the broadcast job ([`broadcast`], design 066), one per send
//! intent, and the expire job ([`expire`], design 068), one per network,
//! which frees what expired pending transactions held.

#[allow(
    clippy::module_inception,
    reason = "the broadcast job beside the expire job"
)]
mod broadcast;
mod expire;

pub(crate) use broadcast::{Broadcast, Payment, SendIntent, progress};
pub(crate) use expire::Expire;
