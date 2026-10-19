//! Pipes (docs/design/065-pipes.md, eval.md). Playground: Select and Check
//! only; actions, runs and the evaluator loop come later.
//!
//! - `reads`: one evaluation's reads, each made once.
//! - `check`: step 2, whether a check or filter holds.
//! - `targets`: what targets name, each resolved once.
//! - `decimals`: each asset's decimals.

mod check;
mod decimals;
mod reads;
mod targets;

pub(crate) use crate::models::trigger::Logic;
pub(crate) use crate::models::{
    trigger::{Aggregate, Amount, Compare, Filter, Gate, Operator, Target, This},
    wallet_index::Selector,
};
pub(crate) use check::Leaf;
pub(crate) use decimals::Decimals;
pub(crate) use reads::{LastRun, Reads};
pub(crate) use targets::Targets;
