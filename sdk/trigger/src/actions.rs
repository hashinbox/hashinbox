//! What a trigger does: the [`Action`] trait, the stored [`Call`] to one,
//! and the [`Registry`] that decodes, checks and runs calls by name. The
//! package has no action of its own; its caller registers them.

use std::collections::BTreeMap;

use anyhow::Context as _;
use base::{Assets, Future, Network};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{
    error::{Class, Error},
    trigger::Context,
};

/// What a trigger does once its check holds: effects of type `E` for its
/// caller to carry out after the round is written, never effects of its own
/// (it runs inside the round's read).
pub trait Action<E>: Send + Sync + 'static {
    /// Its name in `{"name": …, "input": …}`.
    const NAME: &'static str;

    /// Its stored input.
    type Input: DeserializeOwned + Send + Sync;

    /// Refuses an input it could never run, before the trigger is stored.
    ///
    /// # Errors
    ///
    /// The first rule `input` breaks.
    fn validate(
        &self,
        input: &Self::Input,
        network: &Network,
        assets: &Assets,
    ) -> Result<(), Error>;

    /// What to do now, `on.this()` being the kept wallets.
    fn run<'a>(
        &'a self,
        input: &'a Self::Input,
        on: &'a mut Context<'_>,
    ) -> Future<'a, anyhow::Result<Vec<E>>>;
}

/// A stored call to an action: `{"name": "send", "input": {…}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    /// The action's [`Action::NAME`].
    pub name: String,
    /// Its input, decoded by the action's [`Action::Input`].
    #[serde(default)]
    pub input: Value,
}

/// Every action by name, each returning effects of type `E`.
pub struct Registry<E> {
    actions: BTreeMap<&'static str, Box<dyn Erased<E>>>,
}

impl<E> Default for Registry<E> {
    fn default() -> Self {
        Self {
            actions: BTreeMap::new(),
        }
    }
}

impl<E: 'static> Registry<E> {
    /// None yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `action` under its name, replacing one of the same name.
    #[must_use]
    #[expect(
        clippy::should_implement_trait,
        reason = "a builder step, `Registry::new().add(Transfer)`, not addition"
    )]
    pub fn add<A: Action<E>>(mut self, action: A) -> Self {
        self.actions.insert(A::NAME, Box::new(Typed(action)));
        self
    }

    /// Whether `call` names a registered action with an input it accepts.
    ///
    /// # Errors
    ///
    /// An unknown name, an input that does not decode, or the action's
    /// refusal.
    pub fn validate(&self, call: &Call, network: &Network, assets: &Assets) -> Result<(), Error> {
        let Some(action) = self.actions.get(call.name.as_str()) else {
            return Err(unknown(&call.name));
        };
        action.validate(&call.input, network, assets)
    }

    /// What `call` does now over `on`.
    ///
    /// # Errors
    ///
    /// An unknown name, an input that does not decode, or the action's
    /// error.
    pub fn run<'a>(
        &'a self,
        call: &'a Call,
        on: &'a mut Context<'_>,
    ) -> Future<'a, anyhow::Result<Vec<E>>> {
        Box::pin(async move {
            let Some(action) = self.actions.get(call.name.as_str()) else {
                return Err(unknown(&call.name).into());
            };
            action.run(&call.input, on).await
        })
    }
}

/// No action `name` is registered.
fn unknown(name: &str) -> Error {
    Error::new(Class::UnknownAction, format!("unknown action {name}"))
}

/// An action behind its JSON input.
trait Erased<E>: Send + Sync {
    fn validate(&self, input: &Value, network: &Network, assets: &Assets) -> Result<(), Error>;

    fn run<'a>(
        &'a self,
        input: &'a Value,
        on: &'a mut Context<'_>,
    ) -> Future<'a, anyhow::Result<Vec<E>>>;
}

/// An action that decodes its own input.
struct Typed<A>(A);

impl<E, A: Action<E>> Erased<E> for Typed<A> {
    fn validate(&self, input: &Value, network: &Network, assets: &Assets) -> Result<(), Error> {
        let input = A::Input::deserialize(input)
            .map_err(|error| Error::new(Class::BadInput, format!("{} input: {error}", A::NAME)))?;
        self.0.validate(&input, network, assets)
    }

    fn run<'a>(
        &'a self,
        input: &'a Value,
        on: &'a mut Context<'_>,
    ) -> Future<'a, anyhow::Result<Vec<E>>> {
        Box::pin(async move {
            let input =
                A::Input::deserialize(input).with_context(|| format!("{} input", A::NAME))?;
            self.0.run(&input, on).await
        })
    }
}
