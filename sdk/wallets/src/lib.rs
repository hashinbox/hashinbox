#![doc = include_str!("../README.md")]

mod error;
mod factory;
mod pause;
mod queue;
mod sender;
#[cfg(feature = "submissions")]
mod submissions;
// Its own unit tests use the double too, without a dev-dependency on itself.
#[cfg(any(test, feature = "testing"))]
pub mod testing;
mod wallet;
mod wallets;

pub use base::{
    Activation, RegistrationError, RegistrationFuture, SubmissionRegistrar, SubmissionTask,
};
pub use crypto::SecretBytes;
pub use error::{WalletError, WalletErrorKind};
pub use factory::{AddressParser, Dependency, FactoryProfile, Generation, Origin, WalletFactory};
pub use pause::{Admission, Suspension};
pub use queue::{Backoff, Limit, Pause, Policy, Queue, Stats};
pub use sender::{Amount, Authorship, SendError, SendFuture, Sender, Transfer};
#[cfg(feature = "submissions")]
pub use submissions::Submissions;
pub use wallet::{Author, FutureResult, SingleSender, TransactionFactory, Wallet};
pub use wallets::{AssetInfo, MAX_TRANSFERS, Wallets};
