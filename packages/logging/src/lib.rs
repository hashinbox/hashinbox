#![doc = include_str!("../README.md")]

mod logger;
mod memory;

pub use log::{Level, Log, Metadata, Record, kv};
pub use logger::Logger;
pub use memory::{Entry, Filter, Memory, Page};
