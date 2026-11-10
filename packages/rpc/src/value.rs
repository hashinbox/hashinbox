use std::fmt;

use serde::{Serialize, de::DeserializeOwned};

use crate::{Error, ErrorKind};

/// An untyped JSON-RPC result retained until a chain adapter decodes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawJson(pub(crate) Vec<u8>);

impl RawJson {
    /// Wraps bytes after checking they are one valid JSON value.
    pub fn new(bytes: Vec<u8>) -> std::result::Result<Self, Error> {
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|_| Error::new(ErrorKind::InvalidRequest, "raw JSON value is invalid"))?;
        Ok(Self(bytes))
    }

    /// Serializes a value, for test doubles and canned responses.
    pub fn from_serializable<T>(value: &T) -> std::result::Result<Self, Error>
    where
        T: Serialize + ?Sized,
    {
        serde_json::to_vec(value).map(Self).map_err(|_| {
            Error::new(
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
            Error::new(
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

/// The most characters of a node's message a [`Failure`] shows.
const SHOWN: usize = 200;

/// A JSON-RPC error object returned by the server for one call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// JSON-RPC error code.
    pub code: i64,
    /// Server-controlled message; operators see it (truncated) in
    /// [`fmt::Display`], end users should not.
    pub message: String,
    /// Optional server-controlled detail.
    pub data: Option<RawJson>,
}

impl Failure {
    /// Whether the code is in the implementation-defined server range `-32099..=-32000`.
    #[must_use]
    pub const fn is_server_error(&self) -> bool {
        self.code >= -32_099 && self.code <= -32_000
    }

    /// Whether the node refused for a rate or size limit, so the same call
    /// may succeed later: `-32005` ("limit exceeded"), or `-32003` when its
    /// message says "rate limit" or "request limit".
    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        let message = self.message.to_ascii_lowercase();
        match self.code {
            -32_005 => true,
            -32_003 => message.contains("rate limit") || message.contains("request limit"),
            _ => false,
        }
    }
}

/// The code and the node's message, cut to its first 200 characters.
impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let shown: String = self.message.chars().take(SHOWN).collect();
        let cut = if shown.len() < self.message.len() {
            "…"
        } else {
            ""
        };
        write!(
            formatter,
            "JSON-RPC call failed with code {}: {shown}{cut}",
            self.code
        )
    }
}

/// Outcome of one call that reached the server: a result or its error object.
pub type CallResult = std::result::Result<RawJson, Failure>;
