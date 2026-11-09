//! Node doubles: a scripted node for a chain adapter's unit tests
//! ([`Script`]: replies in order, asserts every call, records what was
//! asked), and [`Faults`], which makes any client fail on demand.
//!
//! [`Script`] is a [`Client`](crate::Client), so it substitutes for a composed transport
//! anywhere a chain adapter takes one. The script is the expectation: a
//! call the script does not expect fails the test, as does a call out of
//! order, a call whose parameters differ from the scripted ones, and a
//! script [`Script::exhausted`] finds unconsumed. What a chain keeps for
//! itself is its *fixtures* — the methods, parameters and payloads its node
//! speaks — and the network its client claims.
//!
//! Behind the `testing` feature, which is off by default and belongs in a
//! `[dev-dependencies]` entry only.

mod faults;
mod script;

pub use faults::{Fault, Faults, Switch};
pub use script::{Reply, Script};
