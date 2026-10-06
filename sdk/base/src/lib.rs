#![doc = include_str!("../README.md")]

mod address;
mod asset;
mod block;
mod chain;
mod holder;
mod movement;
mod network;
mod signer;
mod supervision;
/// Signed transactions, their identity, building, and broadcast.
pub mod transaction;
mod unit;

pub use address::Address;
pub use asset::{Asset, AssetId};
pub use block::{
    Block, BlockHash, BlockHeight, BlockParent, BlockPosition, BlockRef, Identified, Transactions,
};
pub use chain::{ChainName, ChainNetwork, ChainNetworks};
pub use crypto::{
    Curve, Digest, PublicKey, PublicKeyFormat, ScalarTweak, SecretBytes, SignRequest,
    SignablePayload, Signature, SignatureEncoding, SignatureScheme, SignedPayload, SignerError,
    SignerErrorKind,
};
pub use decimal::{Decimal, Error as DecimalError};
pub use holder::{Holder, Subject};

pub use movement::MovementKind;
pub use network::{Network, NetworkKind, Networked};
pub use signer::{SignFuture, Signer};
pub use supervision::{
    Activation, RegistrationError, RegistrationFuture, SubmissionRegistrar, SubmissionTask,
};
pub use transaction::{
    Broadcaster, Claim, Draft, Error as TransactionError, ErrorKind as TransactionErrorKind,
    Id as TransactionId, Ref as TransactionRef, SignedTransaction, Snapshot as TransactionSnapshot,
    Submission, TransactionBuilder, TransactionFuture, Transfer,
};
pub use unit::{Unit, Units};

use std::{future::Future, pin::Pin};

/// Boxed `Send` future used by object-safe async capability traits.
///
/// The SDK owns no async runtime and keeps its traits usable as `dyn`, so
/// trait methods return this instead of using `async fn`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
