//! Every scenario states its chains in its own `config.toml`, and nowhere
//! else. `examples/local.rs` is the one file that names a chain crate — three
//! registration lines, the same three `apps/server/src/chains.rs` has — so
//! after setup one `indexing::Index` and one `wallets::Wallets` serve every
//! chain. That is what the examples are there to show, so it is checked rather
//! than reviewed (docs/CHAIN_LAYOUT.md section 8).
//!
//! The chain list comes from `sdk/chains/`, so a new chain crate fails every
//! scenario until each one configures a section for it, and the scenario list
//! comes from the `[[example]]` entries of `examples/Cargo.toml`.
//!
//! `custom_store` is the one scenario whose `main.rs` still names chain
//! crates: it assembles an index by hand instead of from the document, which
//! is its whole subject. That is asserted as an equality rather than skipped,
//! so no other scenario can drift into it and `custom_store` cannot quietly
//! stop being the exception — and where it does name a chain, the old rule
//! still holds: one setup function per chain, named after the chain.

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

    /// Whether this scenario's document configures a chain under its own name.
    fn configures(&self, chain: &str) -> bool {
        self.document
            .lines()
            .any(|line| line.trim() == format!("[{chain}]"))
    }

    /// Every `chain_<name>::` mention with the function it sits in, free or in
    /// an `impl` block. A function ends at the first `}` on its own
    /// indentation, which is what `cargo fmt` guarantees.
    ///
    /// The `::` is required, because naming a chain crate is what the rule is
    /// about: `chain_id` is a key of Ethereum's own configuration section, and
    /// a scenario that reads it names no crate.
    fn mentions(&self) -> Vec<(&str, Option<&str>)> {
        const PREFIX: &str = "chain_";
        let mut found = Vec::new();
        let mut owner: Option<(&str, usize)> = None;
        for line in self.source.lines() {
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
                scenario.configures(&chain),
                "{}/config.toml must configure {chain} under a [{chain}] section",
                scenario.name
            );
        }
    }
}

#[test]
fn only_the_scenario_that_assembles_by_hand_names_a_chain_crate() {
    let naming: BTreeSet<String> = Scenario::all()
        .into_iter()
        .filter(|scenario| !scenario.mentions().is_empty())
        .map(|scenario| scenario.name)
        .collect();
    assert_eq!(
        naming,
        BTreeSet::from(["custom_store".to_owned()]),
        "a scenario states its chains in its config.toml; only custom_store, \
         which assembles its index by hand, may name a chain crate"
    );
}

#[test]
fn a_chain_crate_is_named_only_in_its_own_setup_function() {
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
fn the_shared_setup_library_registers_every_chain_under_its_document_name() {
    let source = fs::read_to_string(Scenario::root().join("local.rs")).expect("local.rs");
    let compact: String = source.split_whitespace().collect();
    for chain in Scenario::chains() {
        let registration = format!("register::<chain_{chain}::Config<Asset>>(\"{chain}\")");
        assert_eq!(
            compact.matches(&registration).count(),
            1,
            "local.rs registers {chain} exactly once, under the name its sections use"
        );
    }
}

#[test]
fn a_mention_is_attributed_to_the_function_that_encloses_it() {
    let scenario = Scenario {
        name: "fixture".to_owned(),
        document: String::new(),
        source: concat!(
            "impl Setup {\n",
            "    async fn bitcoin() {\n",
            "        use chain_bitcoin::Indexer;\n",
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
