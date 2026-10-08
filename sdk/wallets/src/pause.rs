//! One network's pause: new work on it is refused, and the work already
//! admitted is counted until it is answered.
//!
//! [`crate::Wallets::pause`] is the application's half of a quiesced window
//! on one network (the index's online rescan is the other half): every
//! queued send of the network's assets ([`crate::Queue`]) and every
//! [`Admission`] taken with [`crate::Wallets::admit`] counts here.
//! [`Suspension::drained`] resolves once none is left running; dropping the
//! [`Suspension`] admits again. Like the queue, this names no runtime.

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    task::{Context, Poll, Waker},
};

use crate::{WalletError, WalletErrorKind};

/// One network's gate, shared by its assets' queues and its admissions.
#[derive(Default)]
pub(crate) struct Gate(Mutex<State>);

#[derive(Default)]
struct State {
    /// Pauses held; the gate admits nothing while any is.
    pauses: usize,
    /// Admitted work not yet answered.
    running: usize,
    /// Whoever waits for `running` to reach zero.
    drained: Vec<Waker>,
}

/// One admitted piece of work on a network; counted until it is dropped.
#[must_use = "the admission ends when this is dropped"]
pub struct Admission(Arc<Gate>);

/// A paused network: nothing new is admitted on it until this is dropped.
#[must_use = "the network resumes when this is dropped"]
pub struct Suspension(Option<Arc<Gate>>);

impl Gate {
    /// Admits one piece of work, refused while the network is paused.
    pub(crate) fn admit(self: &Arc<Self>) -> Result<Admission, WalletError> {
        let mut state = self.state();
        if state.pauses > 0 {
            return Err(WalletError::new(
                WalletErrorKind::Paused,
                "the asset's network is paused",
            ));
        }
        state.running += 1;
        Ok(Admission(self.clone()))
    }

    /// Refuses new work until the returned value is dropped.
    pub(crate) fn pause(self: &Arc<Self>) -> Suspension {
        self.state().pauses += 1;
        Suspension(Some(self.clone()))
    }

    pub(crate) fn paused(&self) -> bool {
        self.state().pauses > 0
    }

    // Every critical section is one counter step or one waker list edit, so a
    // panic cannot leave the state torn: recovering the guard is sound.
    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Suspension {
    /// A pause of a network nothing is admitted on: no asset is registered
    /// there, so there is nothing to refuse or to wait for.
    pub(crate) const fn nothing() -> Self {
        Self(None)
    }

    /// Resolves once every piece of work admitted before the pause has been
    /// answered. Nothing new is admitted meanwhile, so it resolves.
    pub fn drained(&self) -> impl Future<Output = ()> + Send + '_ {
        Drain(self.0.as_deref())
    }
}

impl Drop for Suspension {
    fn drop(&mut self) {
        if let Some(gate) = &self.0 {
            let mut state = gate.state();
            state.pauses = state.pauses.saturating_sub(1);
        }
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        let wakers = {
            let mut state = self.0.state();
            state.running = state.running.saturating_sub(1);
            if state.running > 0 {
                return;
            }
            std::mem::take(&mut state.drained)
        };
        for waker in wakers {
            waker.wake();
        }
    }
}

/// The wait for a paused network's admitted work.
struct Drain<'a>(Option<&'a Gate>);

impl Future for Drain<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        let Some(gate) = self.0 else {
            return Poll::Ready(());
        };
        let mut state = gate.state();
        if state.running == 0 {
            return Poll::Ready(());
        }
        state.drained.push(context.waker().clone());
        Poll::Pending
    }
}
