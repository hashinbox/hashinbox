//! One scope: a path of resources and an action, as text
//! `com.hashinbox.wallets[id].$read`.

use std::{collections::BTreeSet, fmt, str::FromStr};

use crate::ParseError;

/// One step of a scope's path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Segment {
    /// A resource: all of it (`wallets`), or only the ones named
    /// (`wallets[a,b]`).
    Name {
        /// The resource's name: ASCII letters, digits, `_` and `-`.
        name: String,
        /// The ids it is narrowed to; `None`: every one.
        ids: Option<BTreeSet<String>>,
    },
    /// `*`: any one resource in the middle of a path; at its end, the
    /// resource before it and anything below.
    Any,
}

/// What a scope allows doing.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    /// One action: `$read`.
    Named(String),
    /// `$*`: any action.
    Any,
}

/// One scope: `com.hashinbox.wallets[id].$read`.
///
/// It covers another ([`Scope::covers`]) when its path matches the other's
/// and its action is the same or `$*`. Its text round-trips through
/// [`str::parse`]: ids are escaped (`%`, `[`, `]`, `,` and whitespace as
/// `%XX`), so any id is safe in it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Scope {
    path: Vec<Segment>,
    action: Action,
}

/// Whether `text` is a resource or action name.
fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

impl Segment {
    /// Resource `name`, all of it.
    ///
    /// # Errors
    ///
    /// When `name` is not a name.
    pub fn name(name: &str) -> Result<Self, ParseError> {
        if !is_name(name) {
            return Err(ParseError::Name(name.to_owned()));
        }
        Ok(Self::Name {
            name: name.to_owned(),
            ids: None,
        })
    }

    /// Resource `name`, only `ids`.
    ///
    /// # Errors
    ///
    /// When `name` is not a name or `ids` is empty.
    pub fn ids<I, T>(name: &str, ids: I) -> Result<Self, ParseError>
    where
        I: IntoIterator<Item = T>,
        T: Into<String>,
    {
        let ids: BTreeSet<String> = ids.into_iter().map(Into::into).collect();
        if ids.is_empty() || ids.contains("") {
            return Err(ParseError::Ids(name.to_owned()));
        }
        if !is_name(name) {
            return Err(ParseError::Name(name.to_owned()));
        }
        Ok(Self::Name {
            name: name.to_owned(),
            ids: Some(ids),
        })
    }

    /// The segment `token` is (`scope!`'s): `*`, or a name narrowed to
    /// `ids`.
    #[doc(hidden)]
    #[must_use]
    pub fn __from_token(token: &str, ids: Option<BTreeSet<String>>) -> Self {
        match token {
            "*" => Self::Any,
            name => Self::Name {
                name: name.to_owned(),
                ids,
            },
        }
    }

    /// Whether it covers `other`, one step of a path.
    fn covers(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Any, _) => true,
            (Self::Name { .. }, Self::Any) => false,
            (
                Self::Name { name, ids },
                Self::Name {
                    name: theirs,
                    ids: their_ids,
                },
            ) => {
                name == theirs
                    && match (ids, their_ids) {
                        (None, _) => true,
                        (Some(_), None) => false,
                        (Some(ids), Some(their_ids)) => their_ids.is_subset(ids),
                    }
            }
        }
    }
}

impl Action {
    /// Action `name`.
    ///
    /// # Errors
    ///
    /// When `name` is not a name.
    pub fn named(name: &str) -> Result<Self, ParseError> {
        if !is_name(name) {
            return Err(ParseError::Name(name.to_owned()));
        }
        Ok(Self::Named(name.to_owned()))
    }

    /// Whether it covers `other`: the same, or `$*`.
    fn covers(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Any, _) => true,
            (Self::Named(_), Self::Any) => false,
            (Self::Named(name), Self::Named(theirs)) => name == theirs,
        }
    }
}

/// Whether path `ours` covers path `theirs`: step by step, a `*` at the
/// end covering anything below (nothing included).
fn path_covers(ours: &[Segment], theirs: &[Segment]) -> bool {
    match (ours.split_first(), theirs.split_first()) {
        (None, None) | (Some((Segment::Any, [])), _) => true,
        (Some((step, rest)), Some((their_step, their_rest))) => {
            step.covers(their_step) && path_covers(rest, their_rest)
        }
        _ => false,
    }
}

impl Scope {
    /// `action` on `path`.
    ///
    /// # Errors
    ///
    /// When `path` is empty.
    pub fn new(path: Vec<Segment>, action: Action) -> Result<Self, ParseError> {
        if path.is_empty() {
            return Err(ParseError::Empty);
        }
        Ok(Self { path, action })
    }

    /// `scope!`'s scope: `path` (never empty) and `action` (`*` or a name).
    #[doc(hidden)]
    #[must_use]
    pub fn __from_parts(path: Vec<Segment>, action: &str) -> Self {
        let action = match action {
            "*" => Action::Any,
            name => Action::Named(name.to_owned()),
        };
        Self { path, action }
    }

    /// Its path.
    #[must_use]
    pub fn path(&self) -> &[Segment] {
        &self.path
    }

    /// Its action.
    #[must_use]
    pub const fn action(&self) -> &Action {
        &self.action
    }

    /// Whether it allows everything `other` allows: its action is the same
    /// or `$*`, and its path matches step by step, where
    ///
    /// - a name covers the same name; `wallets` covers `wallets[a]`, and
    ///   `wallets[a,b]` covers `wallets[a]`, not `wallets`;
    /// - `*` in the middle covers any one step;
    /// - `*` at the end covers anything below (`wallets.*` covers
    ///   `wallets`, `wallets[a]` and `wallets[a].legs`).
    ///
    /// A path covers no longer path otherwise: `wallets[a]` does not cover
    /// `wallets[a].legs`.
    #[must_use]
    pub fn covers(&self, other: &Self) -> bool {
        self.action.covers(&other.action) && path_covers(&self.path, &other.path)
    }

    /// It and `other` as one scope, when they differ only in one step's
    /// ids: `wallets[a].$read` and `wallets[b].$read` are
    /// `wallets[a,b].$read`.
    pub(crate) fn merged(&self, other: &Self) -> Option<Self> {
        if self.action != other.action || self.path.len() != other.path.len() {
            return None;
        }
        let differ: Vec<usize> = (0..self.path.len())
            .filter(|&at| self.path[at] != other.path[at])
            .collect();
        let [at] = differ[..] else {
            return None;
        };
        let (
            Segment::Name {
                name,
                ids: Some(ids),
            },
            Segment::Name {
                name: theirs,
                ids: Some(their_ids),
            },
        ) = (&self.path[at], &other.path[at])
        else {
            return None;
        };
        if name != theirs {
            return None;
        }
        let mut merged = self.clone();
        merged.path[at] = Segment::Name {
            name: name.clone(),
            ids: Some(ids.union(their_ids).cloned().collect()),
        };
        Some(merged)
    }

    /// It with its action `action`.
    pub(crate) fn with_action(&self, action: Action) -> Self {
        Self {
            path: self.path.clone(),
            action,
        }
    }
}

/// Bytes an id escapes in its text.
const ESCAPED: &[u8] = b"%[],";

/// `id` as written between `[` and `]`.
fn escape(id: &str, out: &mut fmt::Formatter<'_>) -> fmt::Result {
    for char in id.chars() {
        let escaped = u8::try_from(char).ok().filter(|byte| {
            ESCAPED.contains(byte) || byte.is_ascii_whitespace() || byte.is_ascii_control()
        });
        match escaped {
            Some(byte) => write!(out, "%{byte:02X}")?,
            None => write!(out, "{char}")?,
        }
    }
    Ok(())
}

/// The id `text` (as written between `[` and `]`) is.
fn unescape(text: &str) -> Result<String, ParseError> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            let hex = text
                .get(at + 1..at + 3)
                .ok_or_else(|| ParseError::Escape(text.to_owned()))?;
            let byte =
                u8::from_str_radix(hex, 16).map_err(|_| ParseError::Escape(text.to_owned()))?;
            out.push(byte);
            at += 3;
        } else {
            out.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(out).map_err(|_| ParseError::Escape(text.to_owned()))
}

impl fmt::Display for Segment {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => out.write_str("*"),
            Self::Name { name, ids: None } => out.write_str(name),
            Self::Name {
                name,
                ids: Some(ids),
            } => {
                write!(out, "{name}[")?;
                for (at, id) in ids.iter().enumerate() {
                    if at > 0 {
                        out.write_str(",")?;
                    }
                    escape(id, out)?;
                }
                out.write_str("]")
            }
        }
    }
}

impl fmt::Display for Action {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => out.write_str("$*"),
            Self::Named(name) => write!(out, "${name}"),
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        for step in &self.path {
            write!(out, "{step}.")?;
        }
        write!(out, "{}", self.action)
    }
}

impl FromStr for Segment {
    type Err = ParseError;

    fn from_str(text: &str) -> Result<Self, ParseError> {
        if text == "*" {
            return Ok(Self::Any);
        }
        let Some((name, rest)) = text.split_once('[') else {
            return Self::name(text);
        };
        let ids = rest
            .strip_suffix(']')
            .ok_or_else(|| ParseError::Ids(name.to_owned()))?;
        let ids: Vec<String> = ids.split(',').map(unescape).collect::<Result<_, _>>()?;
        Self::ids(name, ids)
    }
}

impl FromStr for Scope {
    type Err = ParseError;

    /// `text`: steps split by `.` (not inside `[` `]`), the last one the
    /// action, `$name` or `$*`.
    fn from_str(text: &str) -> Result<Self, ParseError> {
        let mut steps = Vec::new();
        let (mut start, mut inside) = (0, false);
        for (at, byte) in text.bytes().enumerate() {
            match byte {
                b'[' if !inside => inside = true,
                b']' if inside => inside = false,
                b'.' if !inside => {
                    steps.push(&text[start..at]);
                    start = at + 1;
                }
                _ => {}
            }
        }
        if inside {
            return Err(ParseError::Ids(text.to_owned()));
        }
        let action = text[start..]
            .strip_prefix('$')
            .ok_or_else(|| ParseError::Action(text.to_owned()))?;
        let action = match action {
            "*" => Action::Any,
            name => Action::named(name)?,
        };
        let path = steps
            .into_iter()
            .map(str::parse)
            .collect::<Result<Vec<Segment>, _>>()?;
        Self::new(path, action)
    }
}

#[cfg(test)]
mod tests {
    use super::Scope;

    fn scope(text: &str) -> Scope {
        text.parse().unwrap()
    }

    #[test]
    fn text_round_trips() {
        for text in [
            "com.hashinbox.wallets.$read",
            "com.hashinbox.wallets[ethereum.mainnet.0xabc].$read",
            "com.hashinbox.wallets[a,b].legs.$read",
            "com.hashinbox.*.$read",
            "com.hashinbox.wallets.*.$*",
        ] {
            assert_eq!(scope(text).to_string(), text);
        }
    }

    #[test]
    fn ids_are_escaped() {
        let odd = super::Segment::ids("wallets", ["a,b]", "c%d"]).unwrap();
        let odd = Scope::new(vec![odd], super::Action::Any).unwrap();
        let text = odd.to_string();
        assert_eq!(text, "wallets[a%2Cb%5D,c%25d].$*");
        assert_eq!(scope(&text), odd);
    }

    #[test]
    fn bad_text_is_refused() {
        for text in [
            "",
            "$read",
            "wallets",
            "wallets.read",
            "wallets[].$read",
            "wallets[a.$read",
            "wal lets.$read",
            "wallets.$re ad",
            "wallets[%zz].$read",
        ] {
            assert!(text.parse::<Scope>().is_err(), "{text}");
        }
    }

    #[test]
    fn coverage() {
        let covers = |ours: &str, theirs: &str| scope(ours).covers(&scope(theirs));
        assert!(
            covers("a.wallets.$read", "a.wallets[x].$read"),
            "all covers one"
        );
        assert!(covers("a.wallets[x,y].$read", "a.wallets[x].$read"));
        assert!(
            !covers("a.wallets[x].$read", "a.wallets.$read"),
            "one, not all"
        );
        assert!(!covers("a.wallets[x].$read", "a.wallets[y].$read"));
        assert!(
            !covers("a.wallets[x].$read", "a.wallets[x].legs.$read"),
            "no children"
        );
        assert!(
            covers("a.*.$read", "a.wallets[x].legs.$read"),
            "trailing: below"
        );
        assert!(
            covers("a.wallets.*.$read", "a.wallets.$read"),
            "trailing: itself too"
        );
        assert!(
            covers("a.*.legs.$read", "a.wallets.legs.$read"),
            "middle: one step"
        );
        assert!(!covers("a.*.legs.$read", "a.wallets.x.legs.$read"));
        assert!(covers("a.wallets.$*", "a.wallets.$write"));
        assert!(!covers("a.wallets.$read", "a.wallets.$*"));
        assert!(!covers("a.wallets.$read", "a.wallets.$write"));
        assert!(!covers("a.wallets.legs.$read", "a.wallets.*.$read"));
    }
}
