use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, de::DeserializeOwned};

use crate::{Chain, Chains, Error, ErrorKind, chain::Endpoints};

/// A document's chain table as it arrives: one named section per chain, none
/// of them decoded yet.
///
/// An application deserializes this wherever its own document keeps the chain
/// sections — `chains` in `apps/server`, the whole file in a scenario that
/// configures nothing else — and hands it to a [`Registry`]. Nothing here
/// knows what a section means.
#[derive(Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct Sections(BTreeMap<String, serde_json::Value>);

impl Sections {
    /// The names this document configured, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// Raw sections for application-owned persistence; may contain credentials.
    pub fn into_values(self) -> BTreeMap<String, serde_json::Value> {
        self.0
    }

    /// Whether the document configured nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A table assembled from named sections, the inverse of
/// [`Sections::into_values`]: an application that keeps part of a section
/// elsewhere (its RPC endpoints, say) hands the amended sections back to a
/// [`Registry`]. A repeated name keeps the last section.
impl FromIterator<(String, serde_json::Value)> for Sections {
    fn from_iter<I: IntoIterator<Item = (String, serde_json::Value)>>(sections: I) -> Self {
        Self(sections.into_iter().collect())
    }
}

impl fmt::Debug for Sections {
    /// Section names only. A section holds endpoint URLs, and a URL may carry
    /// a credential inline — the rule `Rpc`'s `Debug` already follows.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Sections")
            .field("names", &self.0.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// What one registered chain does with its own section's text.
type Parser<A, R> = Box<dyn Fn(&serde_json::Value) -> Result<Box<dyn Chain<A, R>>, Error>>;

/// Which chain parses which section.
///
/// This is the whole of the coupling between a document and the chains it
/// configures: an application names each chain once, in its composition root,
/// and nothing else in the process names one. Adding a fourth chain is one
/// [`Self::register`] line.
///
/// **This crate depends on no chain crate and never will.** Registration is
/// the chain handing itself in: `C` is the chain's own `Config`, and the only
/// thing stored is a closure that deserializes it and boxes it as
/// [`Chain`] — a trait each chain implements for itself.
///
/// `A` is the asset tag the application keys its wallet factories by, and `R`
/// its repository type; both are the application's, and neither is
/// interpreted here.
pub struct Registry<A, R> {
    parsers: BTreeMap<String, Parser<A, R>>,
}

impl<A: 'static, R: 'static> Registry<A, R> {
    /// An empty registry, which accepts no section at all.
    #[must_use]
    pub fn new() -> Self {
        Self {
            parsers: BTreeMap::new(),
        }
    }

    /// Registers `C` as the chain that owns the section called `name`.
    ///
    /// Registering a second chain under one name replaces the first: a
    /// composition root states each name once, and a silent duplicate is a
    /// programming error a document cannot produce.
    pub fn register<C>(&mut self, name: impl Into<String>) -> &mut Self
    where
        C: Chain<A, R> + DeserializeOwned,
    {
        self.parsers.insert(
            name.into(),
            Box::new(|value| {
                let config: C = serde_json::from_value(value.clone())
                    .map_err(|error| Error::new(ErrorKind::Section, error.to_string()))?;
                Ok(Box::new(config))
            }),
        );
        self
    }

    /// Parses every section with the chain registered under its name, and
    /// accepts each one before any of them connects.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Section`] naming the first section no chain is registered
    /// for, the first that does not decode, and the first its own chain
    /// refuses. Every message carries the section it belongs to.
    pub fn chains(&self, sections: &Sections) -> Result<Chains<A, R>, Error> {
        let mut chains = Chains::new();
        for (name, value) in &sections.0 {
            let parse = self
                .parsers
                .get(name)
                .ok_or_else(|| self.unregistered().in_section(name))?;
            let chain = parse(value).map_err(|e| e.in_section(name))?;
            let endpoints = Endpoints::of(value).map_err(|e| e.in_section(name))?;
            chains.push_parsed(name.clone(), chain, endpoints)?;
        }
        Ok(chains)
    }

    /// What an unknown section is measured against: the names this process
    /// was built with, so a typo is answered with the alternatives.
    fn unregistered(&self) -> Error {
        Error::new(
            ErrorKind::Section,
            format!(
                "no chain is registered for this section; this build configures {}",
                self.parsers
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    }
}

impl<A: 'static, R: 'static> Default for Registry<A, R> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use base::BoxFuture;
    use serde::Deserialize;

    use super::*;
    use crate::Rpc;

    /// A chain crate's `Config`, in miniature: its own keys around the shared
    /// shell, and the three methods a registration hook states.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Fixture {
        network: String,
        rpc: Rpc,
        #[serde(default)]
        depth: u64,
    }

    impl Chain<&'static str, ()> for Fixture {
        fn network(&self) -> base::Network {
            base::Network::new(base::ChainName::new("fixture"), self.network.as_str())
        }

        fn accept(&self) -> Result<Option<String>, Error> {
            self.rpc.validate()?;
            if self.depth == 0 {
                return Ok(None);
            }
            Ok(Some(format!("fixture: {} blocks", self.depth)))
        }

        fn verify<'a>(
            &'a self,
            _transport: &'a crate::Transport,
        ) -> BoxFuture<'a, Result<(), Error>> {
            Box::pin(async { Err(Error::refused("a fixture chain contacts no node")) })
        }

        fn connect<'a>(
            &'a self,
            _secrets: &'a dyn json_rpc::Secrets,
            _statistics: &'a mut Vec<json_rpc::Statistics>,
            _logger: logging::Logger,
            _transport: Option<crate::Transport>,
        ) -> BoxFuture<'a, Result<Box<dyn crate::Connection<&'static str, ()>>, Error>> {
            Box::pin(async { Err(Error::refused("a fixture chain contacts no node")) })
        }
    }

    fn registry() -> Registry<&'static str, ()> {
        let mut registry = Registry::new();
        registry.register::<Fixture>("fixture");
        registry
    }

    fn sections(value: serde_json::Value) -> Sections {
        serde_json::from_value(value).expect("a section table")
    }

    fn good() -> serde_json::Value {
        serde_json::json!({"fixture": {
            "network": "one", "rpc": {"endpoints": ["http://127.0.0.1:8545"]}, "depth": 6
        }})
    }

    #[test]
    fn a_registered_section_is_parsed_accepted_and_kept_under_its_name() {
        let chains = registry().chains(&sections(good())).expect("one chain");
        assert_eq!(chains.len(), 1);
        assert_eq!(
            chains.networks(),
            [base::Network::new(base::ChainName::new("fixture"), "one")]
        );
        assert_eq!(chains.notices(), ["fixture: 6 blocks"]);
        assert!(registry().chains(&Sections::default()).is_ok());
    }

    /// The reason the section name is attached by the registry rather than by
    /// the chain: a chain does not know what it was registered as.
    #[test]
    fn every_refusal_names_the_section_it_belongs_to() {
        let unknown = registry()
            .chains(&sections(serde_json::json!({"dogecoin": {}})))
            .expect_err("no chain is registered for it");
        assert_eq!(unknown.kind, ErrorKind::Section);
        assert_eq!(unknown.section.as_deref(), Some("dogecoin"));
        assert!(unknown.to_string().starts_with("dogecoin: "), "{unknown}");
        assert!(unknown.to_string().ends_with("configures fixture"));

        let mut malformed = good();
        malformed["fixture"]["unknown_key"] = serde_json::json!(1);
        let error = registry()
            .chains(&sections(malformed))
            .expect_err("a closed schema");
        assert_eq!(error.section.as_deref(), Some("fixture"));
        assert!(error.message.starts_with("unknown field"), "{error}");

        let mut empty = good();
        empty["fixture"]["rpc"]["endpoints"] = serde_json::json!([]);
        let refused = registry()
            .chains(&sections(empty))
            .expect_err("the chain refuses its own section");
        assert_eq!(
            refused.to_string(),
            "fixture: invalid database or RPC configuration"
        );
    }

    /// Sections taken apart and assembled again parse to the same chains.
    #[test]
    fn sections_reassemble_from_their_values() {
        let original = sections(good());
        let mut values = original.clone().into_values();
        values.get_mut("fixture").expect("the fixture section")["network"] =
            serde_json::json!("two");
        let amended: Sections = values.into_iter().collect();
        assert_eq!(amended.names().collect::<Vec<_>>(), ["fixture"]);
        assert_eq!(
            registry().chains(&amended).expect("one chain").networks(),
            [base::Network::new(base::ChainName::new("fixture"), "two")]
        );
    }

    /// Two chains may not claim one network, and the refusal is reported
    /// before anything connects.
    #[tokio::test]
    async fn one_network_is_claimed_once() {
        let mut both_names = registry();
        both_names.register::<Fixture>("other");
        let mut both = good();
        both["other"] = both["fixture"].clone();
        let error = both_names
            .chains(&sections(both))
            .expect_err("two sections, one network");
        assert_eq!(error.section.as_deref(), Some("other"));
        assert!(error.message.contains("already configured"), "{error}");

        let refused = registry()
            .chains(&sections(good()))
            .expect("one chain")
            .connect(&BTreeMap::new())
            .await
            .err()
            .expect("a fixture chain contacts no node");
        assert_eq!(refused.section.as_deref(), Some("fixture"));
        assert_eq!(
            refused.to_string(),
            "fixture: a fixture chain contacts no node"
        );
    }
}
