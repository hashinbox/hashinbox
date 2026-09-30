use std::fmt;

use serde::de::DeserializeOwned;

use crate::{Error, ErrorKind};

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

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;
    use crate::{Cadence, Rpc};

    /// A chain's section is not this crate's to parse — every chain owns its
    /// own keys — so the test states the shape a chain crate declares.
    #[derive(Deserialize, PartialEq, Eq, Debug)]
    #[serde(deny_unknown_fields)]
    struct Section {
        rpc: Rpc,
        #[serde(default)]
        cadence: Cadence,
    }

    /// The one node every case here reaches, credentials still unresolved.
    const NODE: &str = "http://${BTC_RPC_USER}:${BTC_RPC_PASSWORD}@127.0.0.1:18443";

    #[cfg(feature = "toml")]
    fn toml_text() -> String {
        format!(
            "[rpc]\n\
             endpoints = ['{NODE}']\n\
             repeat = {{ attempts = 3, backoff_ms = 250, max_backoff_ms = 2000 }}\n\
             \n\
             [cadence]\n\
             batch_size = 250\n"
        )
    }

    fn expected() -> Section {
        Section {
            rpc: Rpc::endpoint(NODE),
            cadence: Cadence {
                policy: indexing::SyncPolicy::new(250),
                ..Cadence::new()
            },
        }
    }

    #[cfg(feature = "toml")]
    fn write(name: &str, text: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, text).expect("a writable temporary directory");
        path
    }

    /// The whole point of the feature: the same values, reached from a file,
    /// from a string, and from JSON bytes, with no schema, no defaults and no
    /// validation of the reader's own.
    #[cfg(feature = "toml")]
    #[tokio::test]
    async fn every_way_in_reaches_the_value_a_struct_literal_does() {
        let path = write("config-document-parity.toml", &toml_text());
        let document = Document::read(&path).await.expect("the file is readable");
        let section: Section = document.parse().expect("a TOML section");
        assert_eq!(section, expected());
        section.rpc.validate().expect("one usable node");
        section.cadence.validate().expect("usable bounds");

        assert_eq!(
            Document::toml(toml_text())
                .parse::<Section>()
                .expect("the text"),
            expected(),
            "a string needs no file"
        );

        let json = br#"{"rpc": {"endpoints":
            ["http://${BTC_RPC_USER}:${BTC_RPC_PASSWORD}@127.0.0.1:18443"]},
            "cadence": {"batch_size": 250}}"#;
        assert_eq!(
            Document::json(json)
                .expect("UTF-8")
                .parse::<Section>()
                .expect("a JSON section"),
            expected(),
            "JSON needs no feature"
        );

        // One document, decoded twice: the text is kept, not the value.
        let again: Section = document.parse().expect("the same text");
        assert_eq!(again.rpc.repeat, crate::Repetition::default());
    }

    #[cfg(feature = "toml")]
    #[tokio::test]
    async fn a_missing_file_and_a_malformed_one_are_both_document_failures() {
        let absent = Document::read(std::env::temp_dir().join("config-no-such-file.toml"))
            .await
            .expect_err("the file does not exist");
        assert_eq!(absent.kind, ErrorKind::Document);
        assert!(absent.message.contains("config-no-such-file.toml"));

        let path = write("config-document-broken.toml", "[rpc]\nendpoint = 'a'\n");
        let error = Document::read(&path)
            .await
            .expect("readable")
            .parse::<Section>()
            .expect_err("`endpoint` is not a key");
        assert_eq!(error.kind, ErrorKind::Document);
        assert!(
            error.message.contains("unknown field `endpoint`"),
            "{error}"
        );
    }

    /// JSON needs no feature, so the build without `toml` still has a reader
    /// and the two decoders reach the same value.
    #[test]
    fn json_bytes_decode_without_the_toml_feature() {
        let json = br#"{"rpc": {"endpoints":
            ["http://${BTC_RPC_USER}:${BTC_RPC_PASSWORD}@127.0.0.1:18443"]},
            "cadence": {"batch_size": 250}}"#;
        assert_eq!(
            Document::json(json)
                .expect("UTF-8")
                .parse::<Section>()
                .expect("a JSON section"),
            expected()
        );
        assert!(Document::json(&[0xff]).is_err(), "not UTF-8");
    }

    /// The extension decides the decoder, so one reader serves an operator's
    /// TOML and a test fixture's JSON without a format argument.
    #[cfg(feature = "toml")]
    #[tokio::test]
    async fn the_extension_chooses_the_decoder() {
        let path = write(
            "config-document.json",
            r#"{"rpc": {"endpoints": ["http://127.0.0.1:8545"]}}"#,
        );
        let document = Document::read(&path).await.expect("readable");
        document.parse::<Section>().expect("decoded as JSON");
        assert_eq!(format!("{document:?}"), "Document { bytes: 49 }");
    }
}
