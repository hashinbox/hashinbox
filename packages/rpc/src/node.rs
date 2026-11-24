//! The preset: one node's URL as the client that speaks to it.

use std::sync::Arc;

use crate::{Counted, Endpoint, Error, Http, Split, SplitParams, Throttle, ThrottleParams, Traced};

/// One node as [`node()`] builds it.
pub type Node = Counted<Split<Throttle<Traced<Http>>>>;

/// `endpoint`'s client: [`Http`]; [`Traced`] on it, each request logged;
/// [`Throttle`] and [`Split`], each under the params it reads from the URL,
/// so every request `Split` sends is paced; [`Counted`], so it sees
/// logical calls. Does no I/O.
///
/// # Errors
///
/// An invalid URL or param ([`crate::ErrorKind::InvalidConfiguration`]).
pub fn node(endpoint: &Endpoint) -> Result<Node, Error> {
    let url = endpoint.parse()?;
    let traced = Traced::new(
        Http::new(&url, endpoint.clone())?,
        Arc::new(tracing::Span::none),
    );
    let throttle = Throttle::new(traced, ThrottleParams::try_from(&url)?.rps);
    let split = Split::new(throttle, SplitParams::try_from(&url)?.max_batch);
    Ok(Counted::new(split))
}
