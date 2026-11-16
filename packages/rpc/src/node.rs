//! The preset: one node's URL as the client that speaks to it.

use logging::Logger;

use crate::{
    Counted, Endpoint, Error, Http, Logged, Split, SplitParams, Throttle, ThrottleParams, Url,
};

/// One node as [`node()`] builds it.
pub type Node = Counted<Logged<Split<Throttle<Http>>>>;

/// `endpoint`'s client: [`Http`], [`Throttle`] and [`Split`], each under the
/// params it reads from the URL, so every request `Split` sends is paced;
/// [`Logged`] to `logger` and [`Counted`], so both see logical calls. Does
/// no I/O.
///
/// # Errors
///
/// An invalid URL or param ([`crate::ErrorKind::InvalidConfiguration`]).
pub fn node(endpoint: &Endpoint, logger: Logger) -> Result<Node, Error> {
    let url = Url::parse(&endpoint.url)?;
    let http = Http::new(&url, endpoint.clone())?;
    let throttle = Throttle::new(http, ThrottleParams::try_from(&url)?.rps);
    let split = Split::new(throttle, SplitParams::try_from(&url)?.max_batch);
    Ok(Counted::new(Logged::new(split, logger)))
}
