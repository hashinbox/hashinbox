//! Evaluating a trigger (docs/design/065-pipes.md). Playground: filter and
//! check only; actions and the loop that runs triggers come later.
//!
//! - [`Evaluation`]: whether a trigger's filter or check holds over `This`,
//!   the wallet indexes its selector picked, with balances.
//! - [`Fees`]: a network's fee estimates, each made once.
//! - [`Decimals`]: a network's asset decimals, for display amounts.

mod decimals;
mod evaluation;
mod fees;

pub(crate) use decimals::Decimals;
pub(crate) use evaluation::Evaluation;
pub(crate) use fees::Fees;
