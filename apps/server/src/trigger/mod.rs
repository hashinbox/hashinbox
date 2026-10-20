//! What triggers need to work (docs/design/065-pipes.md, eval.md): each a
//! plain function or type over a SQLite connection; the caller picks the
//! transaction (`Storage::read`, `Storage::write`).
//!
//! - [`touched`]: which triggers a kept block concerns.
//! - [`decide`]: one evaluation per trigger: select, filter, check, plan.
//! - [`preview`]: the same for an unsaved trigger, keeping every figure.
//! - [`run`]: a run started with its send jobs, and settled when confirmed.
//! - [`Evaluation`]: whether a trigger's filter or check holds over `This`,
//!   the wallet indexes its selector picked, with balances.
//! - [`Plan`]: the transfers a trigger's send makes, decided by an evaluation.

mod decide;
mod evaluation;
mod plan;
pub(crate) mod run;
mod touched;

pub(crate) use decide::{Definition, Sources, Verdict, decide, preview};
pub(crate) use evaluation::Evaluation;
pub(crate) use plan::Plan;
#[cfg(test)]
pub(crate) use plan::Planned;
pub(crate) use touched::touched;
