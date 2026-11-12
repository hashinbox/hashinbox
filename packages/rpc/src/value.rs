use serde::{Serialize, de::DeserializeOwned};

use crate::{Error, ErrorKind};

/// An untyped JSON-RPC result retained until an adapter decodes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawJson(pub(crate) Vec<u8>);

impl RawJson {
    /// Wraps bytes after checking they are one valid JSON value.
    pub fn new(bytes: Vec<u8>) -> std::result::Result<Self, Error> {
        serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|_| {
            Error::transport(ErrorKind::InvalidRequest, "raw JSON value is invalid")
        })?;
        Ok(Self(bytes))
    }

    /// Serializes a value, for test doubles and canned responses.
    pub fn from_serializable<T>(value: &T) -> std::result::Result<Self, Error>
    where
        T: Serialize + ?Sized,
    {
        serde_json::to_vec(value).map(Self).map_err(|_| {
            Error::transport(
                ErrorKind::InvalidRequest,
                "JSON-RPC value could not be serialized",
            )
        })
    }

    /// Decodes into `T`; a mismatch is [`ErrorKind::InvalidResponse`].
    pub fn deserialize<T>(&self) -> std::result::Result<T, Error>
    where
        T: DeserializeOwned,
    {
        serde_json::from_slice(&self.0).map_err(|_| {
            Error::transport(
                ErrorKind::InvalidResponse,
                "JSON-RPC result does not match the requested type",
            )
        })
    }

    /// The JSON bytes as received.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Takes the JSON bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}
