//! The decorators: each wraps a [`crate::Client`] and is one itself. One
//! that reads a node's URL starts with its `…Params`, `TryFrom<&Url>`.

mod counted;
mod logged;
mod retry;
mod split;
mod swap;
mod throttle;

pub use counted::{Counted, Snapshot, Statistics};
pub use logged::Logged;
pub use retry::{Backoff, Retry};
pub use split::{Split, SplitParams};
pub use swap::Swap;
pub use throttle::{Throttle, ThrottleParams};
