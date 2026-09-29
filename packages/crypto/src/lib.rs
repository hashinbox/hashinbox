#![doc = include_str!("../README.md")]

mod error;
mod key;
mod key_pair;
mod secret;
mod secret_key;
mod signature;
mod signing;

pub use error::{Error, ErrorKind, SignerError, SignerErrorKind};
pub use key::{Curve, PublicKey, PublicKeyFormat, ScalarTweak};
pub use key_pair::KeyPair;
pub use secret::SecretBytes;
pub use signature::{Signature, SignatureEncoding, SignatureScheme};
pub use signing::{Digest, SignRequest, SignablePayload, SignedPayload};
