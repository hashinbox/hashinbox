//! The decorators: each wraps a [`crate::Client`] and is one itself. One
//! that reads a node's URL starts with its `…Params`, `TryFrom<&Url>`.

mod counted;
mod logged;
mod retry;
mod spanned;
mod split;
mod swap;
mod throttle;
mod traced;

pub use counted::{Counted, Outcome, Snapshot, Statistics};
pub use logged::Logged;
pub use retry::{Backoff, Retry};
pub use spanned::{MakeSpan, Spanned};
pub use split::{Split, SplitParams};
pub use swap::Swap;
pub use throttle::{Throttle, ThrottleParams};
pub use traced::Traced;
