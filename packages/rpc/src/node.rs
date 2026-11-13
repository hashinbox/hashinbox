//! The preset: one node's URL as the client that speaks to it.

use logging::Logger;

use crate::{Counted, Endpoint, Error, Http, Logged, Split, SplitParams, Url};

/// One node as [`node()`] builds it.
pub type Node = Counted<Logged<Split<Http>>>;

/// `endpoint`'s client: [`Http`] and [`Split`], each under the params it
/// reads from the URL, [`Logged`] to `logger` and
/// [`Counted`], so both see logical calls. Does no I/O.
///
/// # Errors
///
/// An invalid URL or param ([`crate::ErrorKind::InvalidConfiguration`]).
pub fn node(endpoint: &Endpoint, logger: Logger) -> Result<Node, Error> {
    let url = Url::parse(&endpoint.url)?;
    let http = Http::new(&url, endpoint.clone())?;
    let split = Split::new(http, SplitParams::try_from(&url)?.max_batch);
    Ok(Counted::new(Logged::new(split, logger)))
}
