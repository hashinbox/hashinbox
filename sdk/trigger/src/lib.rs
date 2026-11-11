//! The trigger engine as composable pieces (docs/design/077-trigger-package.md).
//!
//! A trigger picks wallets ([`selection`]), keeps those its filter keeps, and,
//! when its check holds over them, calls an action ([`actions`]). Filters
//! and checks are [`conditions::Condition`]s, combined by
//! [`conditions::Logic`]; what they compare are [`amounts::Amount`]s,
//! combined by [`amounts::Arithmetic`]. A [`round::Round`] runs every
//! enabled trigger of a network at one confirmed block, over one
//! [`Store`], and returns one [`round::Decision`] per trigger
//! for its caller to write.
//!
//! The trigger language is the caller's: its own enums choose the building
//! blocks ([`conditions::Compare`], [`conditions::Since`],
//! [`amounts::Value`], [`amounts::Balance`], [`amounts::Fee`]) and give the
//! stored JSON its shape. Actions are the caller's too: it registers them in
//! an [`actions::Registry`].

pub mod actions;
pub mod amounts;
pub mod conditions;
pub mod error;
pub mod round;
pub mod selection;
pub mod store;
#[cfg(feature = "testing")]
pub mod testing;
pub mod trigger;

pub use crate::{
    error::{Class, Error},
    round::{Decision, Enabled, LastRound, Round, Stats},
    store::Store,
    trigger::{Context, Trigger},
};
