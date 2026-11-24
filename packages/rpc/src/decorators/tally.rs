//! [`Tally`]: what the requests sent under one piece of work cost.

use std::{cell::Cell, future::Future};

tokio::task_local! {
    /// The tally of the work running on this task, when one is kept.
    static TALLY: Cell<Tally>;
}

/// The requests sent while one piece of work ran on its task, as
/// [`crate::Traced`] logs them: how many, their time on the wire, and their
/// time waiting for the node's rate ([`crate::Throttle`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// Requests sent; a batch is one.
    pub calls: u64,
    /// Their time on the wire, summed, ms.
    pub rpc_ms: u64,
    /// Their time waiting for the rate before they were sent, summed, ms.
    pub wait_ms: u64,
}

impl Tally {
    /// `work`'s output, and the tally of the requests it sent on this task.
    pub async fn of<F: Future>(work: F) -> (F::Output, Self) {
        let counted = async move {
            let output = work.await;
            (output, TALLY.with(Cell::get))
        };
        TALLY.scope(Cell::new(Self::default()), counted).await
    }

    /// Counts one request sent into the tally kept, if any.
    pub(crate) fn add(rpc_ms: u64, wait_ms: u64) {
        let _untallied = TALLY.try_with(|tally| {
            let now = tally.get();
            tally.set(Self {
                calls: now.calls.saturating_add(1),
                rpc_ms: now.rpc_ms.saturating_add(rpc_ms),
                wait_ms: now.wait_ms.saturating_add(wait_ms),
            });
        });
    }
}
