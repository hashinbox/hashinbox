//! A set of scopes: what a key or a token holds, as the JWT `scope` claim
//! (space-separated).

use std::{collections::BTreeMap, fmt, str::FromStr};

use crate::{ParseError, Scope, Segment};

/// Scopes a key or token holds; its text is theirs, space-separated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scopes(Vec<Scope>);

impl Scopes {
    /// None.
    #[must_use]
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// Adds `scope`.
    pub fn push(&mut self, scope: Scope) {
        self.0.push(scope);
    }

    /// Each one.
    pub fn iter(&self) -> std::slice::Iter<'_, Scope> {
        self.0.iter()
    }

    /// How many.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether one of them covers `scope` ([`Scope::covers`]).
    #[must_use]
    pub fn covers(&self, scope: &Scope) -> bool {
        self.0.iter().any(|held| held.covers(scope))
    }

    /// The first of `others` none of them covers; `None` when they cover
    /// every one: `others` only narrows them (a token's scopes against its
    /// key's).
    #[must_use]
    pub fn first_uncovered<'a>(&self, others: &'a Self) -> Option<&'a Scope> {
        others.iter().find(|scope| !self.covers(scope))
    }

    /// [`Self::compress`]ed, then grouped by their shared prefix, the
    /// shortest text for a JWT claim:
    /// `{"com.hashinbox": ["triggers", "wallets[a]:read"]}`. A
    /// prefix is the leading plain names (no ids) every scope
    /// starting with the same step shares, each scope keeping one step at
    /// least. [`Self::unpack`] reverses it.
    #[must_use]
    pub fn pack(&self) -> BTreeMap<String, Vec<String>> {
        let compressed = self.compress();
        let mut by_first: BTreeMap<&Segment, Vec<&Scope>> = BTreeMap::new();
        for scope in &compressed {
            by_first.entry(&scope.path()[0]).or_default().push(scope);
        }
        let mut packed: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for scopes in by_first.into_values() {
            let shared = shared_prefix(&scopes);
            for scope in scopes {
                let (prefix, rest) = scope.path().split_at(shared);
                let rest = scope.with_path(rest.to_vec());
                packed
                    .entry(joined(prefix))
                    .or_default()
                    .push(rest.to_string());
            }
        }
        packed
    }

    /// The scopes [`Self::pack`] packed: each prefix and its scopes joined
    /// by `.` (an empty prefix: the scopes as they are).
    ///
    /// # Errors
    ///
    /// When a joined scope does not parse.
    pub fn unpack(packed: &BTreeMap<String, Vec<String>>) -> Result<Self, ParseError> {
        let each = packed.iter().flat_map(|(prefix, scopes)| {
            scopes.iter().map(move |scope| match prefix.as_str() {
                "" => scope.parse(),
                prefix => format!("{prefix}.{scope}").parse(),
            })
        });
        each.collect()
    }

    /// Them and every scope `depends` adds, to a fixed point, compressed.
    /// `depends` maps a source path (`com.hashinbox.triggers`) to the
    /// scopes holding it needs (`com.hashinbox.wallets`). A held scope
    /// triggers a source when it covers the source or an id of it
    /// (`triggers`, `triggers[t1]`, `com.hashinbox`; not `triggers.runs`),
    /// and adds each target whole: read only when the target is written
    /// `:read` or the held scope is read only. An entry whose text does not
    /// parse adds nothing.
    #[must_use]
    pub fn expand(&self, depends: &[(&str, &[&str])]) -> Self {
        let depends: Vec<(Scope, Self)> = depends
            .iter()
            .filter_map(|(source, targets)| {
                let targets: Result<Self, _> =
                    targets.iter().map(|target| target.parse()).collect();
                Some((source.parse().ok()?, targets.ok()?))
            })
            .collect();
        let mut held = self.0.clone();
        let mut at = 0;
        while at < held.len() {
            let scope = held[at].clone();
            for (source, targets) in &depends {
                if !triggers(&scope, source) {
                    continue;
                }
                for target in targets {
                    let target = target.read_if(scope.is_read());
                    if !held.iter().any(|kept| kept.covers(&target)) {
                        held.push(target);
                    }
                }
            }
            at += 1;
        }
        Self(held).compress()
    }

    /// The fewest scopes allowing the same: one covered by another is
    /// dropped; sorted. What a JWT carries.
    #[must_use]
    pub fn compress(&self) -> Self {
        let mut scopes = without_covered(self.0.clone());
        scopes.sort();
        Self(scopes)
    }
}

/// Whether `held` triggers `source`'s dependencies: it covers `source`,
/// its ids ignored.
fn triggers(held: &Scope, source: &Scope) -> bool {
    let path = held.path().iter().map(|step| Segment {
        name: step.name.clone(),
        id: None,
    });
    held.with_path(path.collect()).reaches(source.path())
}

/// `scopes` but each covered by another (of two equal ones, the first
/// kept).
fn without_covered(scopes: Vec<Scope>) -> Vec<Scope> {
    let dropped = |at: usize| {
        let scope = &scopes[at];
        scopes.iter().enumerate().any(|(other, held)| {
            other != at && held.covers(scope) && (!scope.covers(held) || other < at)
        })
    };
    let kept: Vec<bool> = (0..scopes.len()).map(|at| !dropped(at)).collect();
    let pairs = scopes.into_iter().zip(kept);
    pairs
        .filter(|(_, kept)| *kept)
        .map(|(scope, _)| scope)
        .collect()
}

/// How many leading steps of `scopes` are the same plain name in each,
/// each scope keeping one step at least.
fn shared_prefix(scopes: &[&Scope]) -> usize {
    let shortest = scopes
        .iter()
        .map(|scope| scope.path().len())
        .min()
        .unwrap_or(1);
    let plain = |step: &Segment| step.id.is_none();
    let first = scopes[0].path();
    (0..shortest - 1)
        .take_while(|&at| {
            plain(&first[at]) && scopes.iter().all(|scope| scope.path()[at] == first[at])
        })
        .count()
}

/// `steps` as text, split by `.`.
fn joined(steps: &[Segment]) -> String {
    let names: Vec<String> = steps.iter().map(ToString::to_string).collect();
    names.join(".")
}

impl FromIterator<Scope> for Scopes {
    fn from_iter<I: IntoIterator<Item = Scope>>(scopes: I) -> Self {
        Self(scopes.into_iter().collect())
    }
}

impl IntoIterator for Scopes {
    type Item = Scope;
    type IntoIter = std::vec::IntoIter<Scope>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a Scopes {
    type Item = &'a Scope;
    type IntoIter = std::slice::Iter<'a, Scope>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl fmt::Display for Scopes {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (at, scope) in self.0.iter().enumerate() {
            if at > 0 {
                out.write_str(" ")?;
            }
            write!(out, "{scope}")?;
        }
        Ok(())
    }
}

impl FromStr for Scopes {
    type Err = ParseError;

    /// Space-separated scopes; empty text is none.
    fn from_str(text: &str) -> Result<Self, ParseError> {
        text.split_whitespace().map(str::parse).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::Scopes;

    fn scopes(text: &str) -> Scopes {
        text.parse().unwrap()
    }

    #[test]
    fn a_token_only_narrows_its_key() {
        let key = scopes("a.wallets a.markets:read");
        assert_eq!(key.first_uncovered(&scopes("a.wallets[x]:read")), None);
        let wider = scopes("a.wallets[x]:read a.networks:read");
        assert_eq!(
            key.first_uncovered(&wider).map(ToString::to_string),
            Some("a.networks:read".to_owned())
        );
    }

    #[test]
    fn compress_drops_covered() {
        let held = scopes(
            "a.wallets[x]:read a.wallets[y]:read a.wallets[x]:read a.markets:read \
             a.markets[btc]:read a.wallets[z] a.wallets[z].tags a.networks a.networks:read",
        );
        assert_eq!(
            held.compress().to_string(),
            "a.markets:read a.networks a.wallets[x]:read a.wallets[y]:read a.wallets[z]"
        );
    }

    #[test]
    fn pack_groups_by_the_shared_prefix_and_unpacks() {
        let held = scopes(
            "com.hashinbox.wallets[x]:read com.hashinbox.wallets[y]:read \
             com.hashinbox.wallets[x].tags com.hashinbox.triggers other.thing",
        );
        let packed = held.pack();
        let json: Vec<String> = packed
            .iter()
            .map(|(prefix, scopes)| format!("{prefix}: {}", scopes.join(" ")))
            .collect();
        assert_eq!(
            json,
            [
                "com.hashinbox: triggers wallets[x]:read wallets[x].tags wallets[y]:read",
                "other: thing",
            ]
        );
        assert_eq!(Scopes::unpack(&packed).unwrap(), held.compress());
    }

    #[test]
    fn pack_keeps_one_step_per_scope() {
        let held = scopes("a.b.c:read a.b:read");
        let packed = held.pack();
        assert_eq!(packed["a"], ["b:read"]);
        assert_eq!(Scopes::unpack(&packed).unwrap(), held.compress());
    }

    /// The test dependencies.
    const DEPENDS: &[(&str, &[&str])] = &[
        ("a.triggers", &["a.wallets"]),
        ("a.wallets", &["a.jobs"]),
        ("a.networks.endpoints", &["a.networks:read"]),
    ];

    fn expand(text: &str) -> String {
        scopes(text).expand(DEPENDS).to_string()
    }

    #[test]
    fn expand_adds_dependencies_to_a_fixed_point() {
        assert_eq!(expand("a.triggers"), "a.jobs a.triggers a.wallets");
        assert_eq!(
            expand("a.triggers:read"),
            "a.jobs:read a.triggers:read a.wallets:read"
        );
        assert_eq!(expand("a.triggers[t1]"), "a.jobs a.triggers[t1] a.wallets");
        assert_eq!(expand("a.triggers.runs:read"), "a.triggers.runs:read");
        assert_eq!(
            expand("a.networks[x].endpoints"),
            "a.networks:read a.networks[x].endpoints"
        );
        assert_eq!(expand("a.networks"), "a.networks");
        assert_eq!(expand("a"), "a");
        assert_eq!(expand("a:read"), "a:read");
        assert_eq!(expand("a.wallets:read a.jobs"), "a.jobs a.wallets:read");
        assert_eq!(expand(""), "");
    }
}
