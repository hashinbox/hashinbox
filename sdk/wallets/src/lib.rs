#![doc = include_str!("../README.md")]

mod error;
mod factories;
mod factory;
pub mod storage;
mod wallet;

pub use crypto::SecretBytes;
pub use error::{Error, ErrorKind};
pub use factories::{Factories, FactoryBroadcaster};
pub use factory::Factory;
pub use storage::WalletsStore;
pub use wallet::{FutureResult, Wallet};
