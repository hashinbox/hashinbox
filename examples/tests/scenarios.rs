//! Every scenario is self-contained: its `main.rs` names the chains the
//! application links and its `config.toml` configures them. Which chains an
//! application links is its own decision, so the two halves are written in two
//! places, and what is checked here is that they agree: a scenario composes
//! exactly the chains its own document configures, no more and no fewer.
//!
//! The chain list comes from `sdk/chains/`, so a new chain crate fails every
//! scenario until each one configures a section for it, and the scenario list
//! comes from the `[[example]]` entries of `examples/Cargo.toml`.
//!
//! A scenario hands its sections to `payments::config::Registry`, one
//! `register::<chain_<name>::Config<_>>("<name>")` (or the container's
//! `.with::<chain_<name>::Config<_>>("<name>")`) line per chain.
//! `custom_store` is the one scenario that registers nothing: it assembles an
//! index by hand, which is its whole subject. That is asserted as an equality
//! rather than skipped, so no other scenario can drift into it, and where it
//! names a chain the rule is one setup function per chain, named after the
//! chain.

use std::{collections::BTreeSet, fs, path::Path};

/// One scenario: the `main.rs` its `[[example]]` names, and the `config.toml`
/// beside it that states its chains.
struct Scenario {
    name: String,
    source: String,
    document: String,
}

impl Scenario {
    /// Every scenario of `examples/Cargo.toml`, in manifest order.
    fn all() -> Vec<Self> {
        let manifest = fs::read_to_string(Self::root().join("Cargo.toml")).expect("Cargo.toml");
        let mut scenarios = Vec::new();
        for line in manifest.lines() {
            let Some(path) = line
                .strip_prefix("path = \"")
                .and_then(|rest| rest.strip_suffix('"'))
            else {
                continue;
            };
            let Some(name) = path.strip_suffix("/main.rs") else {
                continue;
            };
            scenarios.push(Self {
                name: name.to_owned(),
                source: Self::read(path),
                document: Self::read(&format!("{name}/config.toml")),
            });
        }
        assert!(!scenarios.is_empty(), "no [[example]] names a scenario");
        scenarios
    }

    /// One file of the examples package, named by the failure if it is absent.
    fn read(path: &str) -> String {
        fs::read_to_string(Self::root().join(path))
            .unwrap_or_else(|error| panic!("{path}: {error}"))
    }

    /// The examples package directory.
    fn root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
    }

    /// Every chain crate of the workspace, by the name of its directory.
    fn chains() -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        for entry in fs::read_dir(Self::root().join("../sdk/chains")).expect("sdk/chains") {
            let entry = entry.expect("chain crate directory");
            if entry.path().is_dir() {
                names.insert(entry.file_name().to_string_lossy().into_owned());
            }
        }
        assert!(!names.is_empty(), "sdk/chains holds no chain crate");
        names
    }

    /// The sections this scenario's document configures: every top-level
    /// table header. `[solana.rpc]` is a table inside a section and
    /// `[[tokens]]` is an array of them, so neither is a section.
    fn configured(&self) -> BTreeSet<&str> {
        self.document
            .lines()
            .filter_map(|line| line.trim().strip_prefix('[')?.strip_suffix(']'))
            .filter(|name| !name.is_empty() && name.chars().all(Self::continues))
            .collect()
    }

    /// Every `register::<chain_<crate>::Config<_>>("<section>")` of the
    /// source, as the crate it names and the section it registers it for, in
    /// source order. Comment lines register nothing.
    fn registrations(&self) -> Vec<(&str, &str)> {
        let mut found = Vec::new();
        for line in self.code() {
            // A `Registry` registration, or the container's `.with::<..>(..)`.
            let Some((_, rest)) = line
                .split_once("register::<chain_")
                .or_else(|| line.split_once("with::<chain_"))
            else {
                continue;
            };
            let section = rest
                .split_once(">>(\"")
                .and_then(|(_, quoted)| quoted.split_once('"'));
            let Some((section, _)) = section else {
                continue;
            };
            found.push((Self::word(rest), section));
        }
        found
    }

    /// The source without its comment lines.
    fn code(&self) -> impl Iterator<Item = &str> {
        self.source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
    }

    /// Every `chain_<name>::` mention outside a registration, with the
    /// function it sits in, free or in an `impl` block. A function ends at the
    /// first `}` on its own indentation, which is what `cargo fmt` guarantees.
    ///
    /// The `::` is required, because naming a chain crate is what the rule is
    /// about: `chain_id` is a key of Ethereum's own configuration section, and
    /// a scenario that reads it names no crate.
    fn mentions(&self) -> Vec<(&str, Option<&str>)> {
        const PREFIX: &str = "chain_";
        let mut found = Vec::new();
        let mut owner: Option<(&str, usize)> = None;
        for line in self.code() {
            if line.contains("register::<chain_") || line.contains("with::<chain_") {
                continue;
            }
            owner = Self::signature(line).or(owner);
            let name = owner.map(|(name, _)| name);
            found.extend(line.match_indices(PREFIX).filter_map(|(at, _)| {
                let rest = &line[at + PREFIX.len()..];
                let crate_name = Self::word(rest);
                let joined = line[..at].ends_with(|letter: char| Self::continues(letter));
                let path = rest[crate_name.len()..].starts_with("::");
                (!joined && path).then_some((crate_name, name))
            }));
            owner = owner.filter(|(_, indent)| line != format!("{:indent$}}}", ""));
        }
        found
    }

    /// The name and indentation of a `fn` or `async fn` signature.
    fn signature(line: &str) -> Option<(&str, usize)> {
        let indent = line.len() - line.trim_start().len();
        let rest = line
            .trim_start()
            .strip_prefix("async fn ")
            .or_else(|| line.trim_start().strip_prefix("fn "))?;
        Some((Self::word(rest), indent))
    }

    /// The identifier at the start of `text`.
    fn word(text: &str) -> &str {
        let end = text
            .find(|letter: char| !Self::continues(letter))
            .unwrap_or(text.len());
        &text[..end]
    }

    fn continues(letter: char) -> bool {
        letter.is_alphanumeric() || letter == '_'
    }
}

#[test]
fn every_scenario_configures_every_chain() {
    for scenario in Scenario::all() {
        for chain in Scenario::chains() {
            assert!(
                scenario.configured().contains(chain.as_str()),
                "{}/config.toml must configure {chain} under a [{chain}] section",
                scenario.name
            );
        }
    }
}

#[test]
fn every_scenario_composes_exactly_the_chains_it_configures() {
    for scenario in Scenario::all() {
        let registered = scenario.registrations();
        let assembled = scenario.mentions();
        let composed: BTreeSet<&str> = registered
            .iter()
            .map(|(chain, _)| *chain)
            .chain(assembled.iter().map(|(chain, _)| *chain))
            .collect();
        assert_eq!(
            composed,
            scenario.configured(),
            "{}/main.rs must compose exactly the chains {0}/config.toml configures, \
             no more and no fewer",
            scenario.name
        );
    }
}

#[test]
fn a_registration_names_each_configured_chain_once_under_its_section() {
    for scenario in Scenario::all() {
        let mut registered = scenario.registrations();
        if registered.is_empty() {
            continue;
        }
        registered.sort_unstable();
        let configured: Vec<_> = scenario
            .configured()
            .into_iter()
            .map(|section| (section, section))
            .collect();
        assert_eq!(
            registered, configured,
            "{}/main.rs registers each chain {0}/config.toml configures exactly once, \
             as register::<chain_<name>::Config<_>>(\"<name>\")",
            scenario.name
        );
    }
}

#[test]
fn only_the_scenario_that_assembles_by_hand_registers_nothing() {
    let assembling: BTreeSet<String> = Scenario::all()
        .into_iter()
        .filter(|scenario| scenario.registrations().is_empty())
        .map(|scenario| scenario.name)
        .collect();
    assert_eq!(
        assembling,
        BTreeSet::from(["custom_store".to_owned()]),
        "a scenario registers its chains; only custom_store, which assembles \
         its index by hand, may name a chain crate any other way"
    );
}

#[test]
fn outside_a_registration_a_chain_crate_is_named_only_in_its_own_setup_function() {
    for scenario in Scenario::all() {
        let stray: Vec<_> = scenario
            .mentions()
            .into_iter()
            .filter(|(name, owner)| *owner != Some(*name))
            .collect();
        assert!(
            stray.is_empty(),
            "{}/main.rs names a chain crate outside its setup function: {stray:?}",
            scenario.name
        );
    }
}

#[test]
fn a_registration_is_read_with_its_crate_and_its_section() {
    let scenario = Scenario {
        name: "fixture".to_owned(),
        document: concat!(
            "# [commented]\n",
            "[bitcoin]\n",
            "rpc.endpoints = ['http://127.0.0.1:1']\n",
            "[solana]\n",
            "[solana.rpc]\n",
            "[[solana.tokens]]\n",
        )
        .to_owned(),
        source: concat!(
            "fn main() {\n",
            "    registry.register::<chain_bitcoin::Config<String>>(\"bitcoin\");\n",
            "    // registry.register::<chain_solana::Config<String>>(\"solana\");\n",
            "    registry.register::<chain_tron::Config<Asset>>(\"solana\");\n",
            "}\n",
        )
        .to_owned(),
    };
    assert_eq!(scenario.configured(), BTreeSet::from(["bitcoin", "solana"]));
    assert_eq!(
        scenario.registrations(),
        [("bitcoin", "bitcoin"), ("tron", "solana")]
    );
    assert_eq!(scenario.mentions(), []);
}

#[test]
fn a_mention_is_attributed_to_the_function_that_encloses_it() {
    let scenario = Scenario {
        name: "fixture".to_owned(),
        document: String::new(),
        source: concat!(
            "impl Setup {\n",
            "    async fn bitcoin() {\n",
            "        use chain_bitcoin::indexer;\n",
            "    }\n",
            "}\n",
            "\n",
            "fn main() {\n",
            "    let _ = chain_solana::SOL;\n",
            "}\n",
            "\n",
            "static AFTERWARDS: Option<chain_tron::Config> = None;\n",
        )
        .to_owned(),
    };
    assert_eq!(
        scenario.mentions(),
        [
            ("bitcoin", Some("bitcoin")),
            ("solana", Some("main")),
            ("tron", None),
        ]
    );
}
