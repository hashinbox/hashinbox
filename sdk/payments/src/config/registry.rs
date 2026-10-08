use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, de::DeserializeOwned};

use crate::config::{Chain, Chains, Error, ErrorKind, chain::Endpoints};

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
