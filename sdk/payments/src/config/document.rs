use std::fmt;

use serde::de::DeserializeOwned;

use crate::config::{Error, ErrorKind};

/// A configuration document's text, read once and decoded on demand.
///
/// This is a convenience, not the API. Every type this crate exports and every
/// `Config` a chain exports is plain data with public fields and a `new`
/// constructor: a deployment that already parses YAML or environment variables
/// deserializes them with its own reader, and one that has its values in hand
/// writes them as struct literals and never builds a `Document` at all.
///
/// The text is kept rather than the decoded value so that a decode failure
/// carries the parser's own line and column, and so that one document can be
/// decoded into more than one type.
#[derive(Clone, PartialEq, Eq)]
pub struct Document {
    text: String,
    /// Whether [`Self::parse`] decodes the text as TOML rather than JSON.
    #[cfg(feature = "toml")]
    toml: bool,
}

impl Document {
    /// A JSON document, for an embedder that already holds the bytes.
    ///
    /// This needs no feature: JSON is `serde_json`, which this crate links to
    /// hold a section it has not yet handed to a chain.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Document`] when the bytes are not UTF-8.
    pub fn json(bytes: &[u8]) -> Result<Self, Error> {
        let text = String::from_utf8(bytes.to_vec())
            .map_err(|error| Error::new(ErrorKind::Document, error.to_string()))?;
        Ok(Self {
            text,
            #[cfg(feature = "toml")]
            toml: false,
        })
    }

    /// A TOML document, from text a caller already has.
    #[cfg(feature = "toml")]
    #[must_use]
    pub fn toml(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            toml: true,
        }
    }

    /// Reads a configuration file: TOML when the path ends in `.toml`, JSON
    /// otherwise. Nothing is decoded and nothing is validated yet.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Document`] when the file cannot be read; the message
    /// names the path, which is an operator's own argument and not a secret.
    #[cfg(feature = "toml")]
    pub async fn read(path: impl AsRef<std::path::Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let text = tokio::fs::read_to_string(path).await.map_err(|error| {
            Error::new(ErrorKind::Document, format!("{}: {error}", path.display()))
        })?;
        let toml = path
            .extension()
            .is_some_and(|extension| extension == "toml");
        Ok(Self { text, toml })
    }

    /// Decodes the document into `T`.
    ///
    /// Defaults and strictness are `T`'s own, and so is validation: this
    /// performs none, because a value built in code must reach exactly the
    /// same checks.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Document`] when the text does not describe a `T`; the
    /// message carries the parser's line and column.
    pub fn parse<T: DeserializeOwned>(&self) -> Result<T, Error> {
        #[cfg(feature = "toml")]
        if self.toml {
            return toml::from_str(&self.text)
                .map_err(|error| Error::new(ErrorKind::Document, error.to_string()));
        }
        serde_json::from_str(&self.text)
            .map_err(|error| Error::new(ErrorKind::Document, error.to_string()))
    }
}

impl fmt::Debug for Document {
    /// The text is the deployment's whole configuration; only its size is
    /// printed, for the reason `Rpc`'s `Debug` is written rather than derived.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Document")
            .field("bytes", &self.text.len())
            .finish()
    }
}
