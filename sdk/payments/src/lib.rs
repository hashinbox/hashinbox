#![doc = include_str!("../README.md")]

mod config;
mod payments;

pub use config::Document;
pub use payments::{Builder, Payments};
