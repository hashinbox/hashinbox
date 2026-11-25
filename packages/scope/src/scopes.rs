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
    /// `{"com.hashinbox": ["markets.$read", "wallets[a,b].$read"]}`. A
    /// prefix is the leading plain names (no ids, no `*`) every scope
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
                let rest =
                    Scope::__from_parts(rest.to_vec(), "*").with_action(scope.action().clone());
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

    /// The fewest scopes allowing the same: one covered by another is
    /// dropped, and scopes differing only in one step's ids are merged
    /// (`wallets[a].$read wallets[b].$read` is `wallets[a,b].$read`);
    /// sorted. What a JWT carries.
    #[must_use]
    pub fn compress(&self) -> Self {
        let mut scopes = self.0.clone();
        loop {
            let before = scopes.len();
            scopes = without_covered(scopes);
            scopes = merged(scopes);
            if scopes.len() == before {
                break;
            }
        }
        scopes.sort();
        Self(scopes)
    }
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

/// `scopes` with each pair differing only in one step's ids merged.
fn merged(scopes: Vec<Scope>) -> Vec<Scope> {
    let mut out: Vec<Scope> = Vec::with_capacity(scopes.len());
    for scope in scopes {
        match out.iter().position(|kept| kept.merged(&scope).is_some()) {
            Some(at) => {
                if let Some(merged) = out[at].merged(&scope) {
                    out[at] = merged;
                }
            }
            None => out.push(scope),
        }
    }
    out
}

/// How many leading steps of `scopes` are the same plain name in each,
/// each scope keeping one step at least.
fn shared_prefix(scopes: &[&Scope]) -> usize {
    let shortest = scopes
        .iter()
        .map(|scope| scope.path().len())
        .min()
        .unwrap_or(1);
    let plain = |step: &Segment| matches!(step, Segment::Name { ids: None, .. });
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
        let key = scopes("a.wallets.$* a.markets.$read");
        assert_eq!(key.first_uncovered(&scopes("a.wallets[x].$read")), None);
        let wider = scopes("a.wallets[x].$read a.networks.$read");
        assert_eq!(
            key.first_uncovered(&wider).map(ToString::to_string),
            Some("a.networks.$read".to_owned())
        );
    }

    #[test]
    fn compress_drops_covered_and_merges_ids() {
        let held = scopes(
            "a.wallets[x].$read a.wallets[y].$read a.wallets[x].$read \
             a.markets.$read a.markets[btc].$read a.wallets[z].$write",
        );
        assert_eq!(
            held.compress().to_string(),
            "a.markets.$read a.wallets[x,y].$read a.wallets[z].$write"
        );
    }

    #[test]
    fn pack_groups_by_the_shared_prefix_and_unpacks() {
        let held = scopes(
            "com.hashinbox.wallets[x].$read com.hashinbox.wallets[y].$read \
             com.hashinbox.wallets[x].legs.$read com.hashinbox.markets.$read other.thing.$write",
        );
        let packed = held.pack();
        let json: Vec<String> = packed
            .iter()
            .map(|(prefix, scopes)| format!("{prefix}: {}", scopes.join(" ")))
            .collect();
        assert_eq!(
            json,
            [
                "com.hashinbox: markets.$read wallets[x].legs.$read wallets[x,y].$read",
                "other: thing.$write",
            ]
        );
        assert_eq!(Scopes::unpack(&packed).unwrap(), held.compress());
    }

    #[test]
    fn pack_keeps_one_step_per_scope() {
        let held = scopes("a.b.c.$read a.b.$read");
        let packed = held.pack();
        assert_eq!(packed["a"], ["b.$read", "b.c.$read"]);
        assert_eq!(Scopes::unpack(&packed).unwrap(), held.compress());
    }

    #[test]
    fn compress_keeps_what_is_allowed() {
        let held = scopes("a.wallets[x].$read a.wallets[y].$read a.*.$write a.wallets.$write");
        let compressed = held.compress();
        assert_eq!(compressed.to_string(), "a.wallets[x,y].$read a.*.$write");
        for scope in &held {
            assert!(compressed.covers(scope), "{scope}");
        }
    }
}
