//! [`Document`]: the configuration read from JSON, TOML or a value.

use std::fmt;

use anyhow::Context as _;
use serde::Deserialize;
use serde_json::{Map, Value};

/// The configuration document: one section per network, each naming its
/// `chain`; the rest of a section is that chain's own `base::Config`
/// (its `interval_ms` included).
///
/// ```json
/// { "networks": [{ "chain": "bitcoin", "network": "regtest", "endpoints": [] }] }
/// ```
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    /// Network sections, each with its `chain`.
    #[serde(default)]
    pub networks: Vec<Map<String, Value>>,
}

impl Document {
    /// Reads a document from a value, such as one table of an application's
    /// own configuration.
    ///
    /// # Errors
    ///
    /// When the value is not a document.
    pub fn from_value(value: Value) -> anyhow::Result<Self> {
        serde_json::from_value(value).context("payments configuration")
    }

    /// Reads a JSON document.
    ///
    /// # Errors
    ///
    /// When the text is not a document; the message carries line and column.
    pub fn from_json(text: &str) -> anyhow::Result<Self> {
        serde_json::from_str(text).context("payments configuration")
    }

    /// Reads a TOML document.
    ///
    /// # Errors
    ///
    /// When the text is not a document; the message carries line and column.
    #[cfg(feature = "toml")]
    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        toml::from_str(text).context("payments configuration")
    }

    /// Each section as its chain name and its chain's own configuration.
    pub(crate) fn sections(&self) -> anyhow::Result<Vec<(String, Value)>> {
        self.networks
            .iter()
            .enumerate()
            .map(|(index, section)| {
                let mut section = section.clone();
                let chain = match section.remove("chain") {
                    Some(Value::String(chain)) => chain,
                    _ => anyhow::bail!("network section {index} names no `chain`"),
                };
                Ok((chain, Value::Object(section)))
            })
            .collect()
    }
}

impl fmt::Debug for Document {
    /// Chain names only: a section's endpoint URLs may carry credentials.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let chains = self
            .networks
            .iter()
            .map(|section| section.get("chain"))
            .collect::<Vec<_>>();
        formatter
            .debug_struct("Document")
            .field("chains", &chains)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_split_the_chain_from_its_configuration() {
        let document = Document::from_json(
            r#"{"networks": [{"chain": "fake", "network": "a"}, {"network": "b"}]}"#,
        )
        .unwrap();
        assert!(document.sections().is_err());

        let document = Document::from_json(r#"{"networks": [{"chain": "fake", "x": 1}]}"#).unwrap();
        let sections = document.sections().unwrap();
        assert_eq!(sections, [("fake".to_owned(), serde_json::json!({"x": 1}))]);
    }

    #[test]
    fn rejects_unknown_fields_and_hides_sections() {
        assert!(Document::from_json(r#"{"interval_ms": 5}"#).is_err());
        let document =
            Document::from_json(r#"{"networks": [{"chain": "fake", "url": "http://u:p@h"}]}"#)
                .unwrap();
        assert!(!format!("{document:?}").contains("u:p"));
    }

    #[cfg(feature = "toml")]
    #[test]
    fn reads_toml() {
        let document =
            Document::from_toml("[[networks]]\nchain = \"fake\"\nnetwork = \"a\"\n").unwrap();
        assert_eq!(document.sections().unwrap().len(), 1);
    }
}
