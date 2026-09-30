#![doc = include_str!("../README.md")]

mod balance;
mod composition;
mod error;
mod factory;
mod history;
mod pending;
mod sender;
mod spendable;
#[cfg(feature = "submissions")]
mod submissions;
mod wallet;
mod wallets;

pub use balance::Balances;
pub use base::{
    Activation, RegistrationError, RegistrationFuture, SubmissionRegistrar, SubmissionTask,
};
pub use composition::{ComposedChain, Composition, Connection};
pub use crypto::SecretBytes;
pub use error::{WalletError, WalletErrorKind};
pub use factory::{AddressParser, Dependency, FactoryProfile, Generation, Origin, WalletFactory};
pub use history::{History, ObservedTransaction, TransactionPage, TransactionStatus};
pub use indexing::{Holder, Page, Subject};
pub use pending::PendingJoin;
pub use sender::{Amount, Authorship, SendError, SendFuture, Sender, Transfer};
#[cfg(feature = "submissions")]
pub use submissions::Submissions;
pub use wallet::{Author, FutureResult, SingleSender, TransactionFactory, Wallet};
pub use wallets::{AssetInfo, MAX_TRANSFERS, Wallets};
