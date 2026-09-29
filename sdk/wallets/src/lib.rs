#![doc = include_str!("../README.md")]

mod composition;
mod error;
mod factory;
mod sender;
mod wallet;
mod wallets;

pub use base::{
    Activation, RegistrationError, RegistrationFuture, SubmissionRegistrar, SubmissionTask,
};
pub use composition::{ComposedChain, Composition, Connection};
pub use crypto::SecretBytes;
pub use error::{WalletError, WalletErrorKind};
pub use factory::{AddressParser, Dependency, FactoryProfile, Generation, Origin, WalletFactory};
pub use indexing::{Holder, Page, Subject};
pub use sender::{Authorship, SendError, SendFuture, Sender, Transfer};
pub use wallet::{Author, FutureResult, SingleSender, TransactionFactory, Wallet};
pub use wallets::{AssetInfo, MAX_TRANSFERS, Wallets};
