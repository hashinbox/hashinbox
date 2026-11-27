//! One scope: a path of resources, full or read only, as text
//! `com.hashinbox.wallets[id]:read`.

use std::{fmt, str::FromStr};

use crate::ParseError;

/// One step of a scope's path: a resource, all of it (`wallets`), or only
/// the one named (`wallets[a]`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Segment {
    /// The resource's name: ASCII letters, digits, `_` and `-`.
    pub name: String,
    /// The id it is narrowed to; `None`: every one.
    pub id: Option<String>,
}

/// One scope: `com.hashinbox.wallets[id]` (full: read and write) or
/// `com.hashinbox.wallets[id]:read` (read only).
///
/// It covers ([`Scope::covers`]) its whole subtree. Its text round-trips
/// through [`str::parse`]: ids are escaped (`%`, `[`, `]` and whitespace
/// as `%XX`), so any id is safe in it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Scope {
    path: Vec<Segment>,
    read: bool,
}

/// The suffix of a read-only scope.
const READ: &str = ":read";

/// Whether `text` is a resource name.
fn is_name(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

impl Segment {
    /// Resource `name`, only `id` when it has one.
    ///
    /// # Errors
    ///
    /// When `name` is not a name or `id` is empty.
    pub fn new(name: &str, id: Option<String>) -> Result<Self, ParseError> {
        if id.as_deref() == Some("") {
            return Err(ParseError::Id(name.to_owned()));
        }
        if !is_name(name) {
            return Err(ParseError::Name(name.to_owned()));
        }
        Ok(Self {
            name: name.to_owned(),
            id,
        })
    }

    /// Whether it covers `other`, one step of a path: the same name, and
    /// no id or the same.
    fn covers(&self, other: &Self) -> bool {
        self.name == other.name && (self.id.is_none() || self.id == other.id)
    }
}

impl Scope {
    /// `path`, read only when `read`.
    ///
    /// # Errors
    ///
    /// When `path` is empty.
    pub fn new(path: Vec<Segment>, read: bool) -> Result<Self, ParseError> {
        if path.is_empty() {
            return Err(ParseError::Empty);
        }
        Ok(Self { path, read })
    }

    /// Its path.
    #[must_use]
    pub fn path(&self) -> &[Segment] {
        &self.path
    }

    /// Whether it is read only (`:read`); otherwise full.
    #[must_use]
    pub const fn is_read(&self) -> bool {
        self.read
    }

    /// Whether it allows everything `other` allows: it is full or `other`
    /// is read only, and its path is a prefix of `other`'s, step by step
    /// (`wallets` covers `wallets[a]`, `wallets[a]` only `wallets[a]`).
    /// A scope so covers its whole subtree: `networks` covers
    /// `networks[x].endpoints[y]`; a child never covers its parent.
    #[must_use]
    pub fn covers(&self, other: &Self) -> bool {
        (!self.read || other.read) && self.reaches(&other.path)
    }

    /// Whether its path is a prefix of `path`, step by step.
    pub(crate) fn reaches(&self, path: &[Segment]) -> bool {
        self.path.len() <= path.len()
            && self
                .path
                .iter()
                .zip(path)
                .all(|(ours, theirs)| ours.covers(theirs))
    }

    /// It with path `path`.
    pub(crate) fn with_path(&self, path: Vec<Segment>) -> Self {
        Self {
            path,
            read: self.read,
        }
    }

    /// It read only when `read`, else as it is.
    pub(crate) fn read_if(&self, read: bool) -> Self {
        Self {
            path: self.path.clone(),
            read: self.read || read,
        }
    }
}

/// Bytes an id escapes in its text.
const ESCAPED: &[u8] = b"%[]";

/// `id` as written between `[` and `]`.
pub(crate) fn escape(id: &str) -> String {
    let mut text = String::with_capacity(id.len());
    for char in id.chars() {
        let escaped = u8::try_from(char).ok().filter(|byte| {
            ESCAPED.contains(byte) || byte.is_ascii_whitespace() || byte.is_ascii_control()
        });
        match escaped {
            Some(byte) => text.push_str(&format!("%{byte:02X}")),
            None => text.push(char),
        }
    }
    text
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
        match &self.id {
            None => out.write_str(&self.name),
            Some(id) => write!(out, "{}[{}]", self.name, escape(id)),
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (at, step) in self.path.iter().enumerate() {
            if at > 0 {
                out.write_str(".")?;
            }
            write!(out, "{step}")?;
        }
        if self.read {
            out.write_str(READ)?;
        }
        Ok(())
    }
}

impl FromStr for Segment {
    type Err = ParseError;

    fn from_str(text: &str) -> Result<Self, ParseError> {
        let Some((name, rest)) = text.split_once('[') else {
            return Self::new(text, None);
        };
        let id = rest
            .strip_suffix(']')
            .ok_or_else(|| ParseError::Id(name.to_owned()))?;
        Self::new(name, Some(unescape(id)?))
    }
}

impl FromStr for Scope {
    type Err = ParseError;

    /// `text`: steps split by `.` (not inside `[` `]`), then `:read` or
    /// nothing.
    fn from_str(text: &str) -> Result<Self, ParseError> {
        let mut steps = Vec::new();
        let (mut start, mut inside, mut end) = (0, false, text.len());
        for (at, byte) in text.bytes().enumerate() {
            match byte {
                b'[' if !inside => inside = true,
                b']' if inside => inside = false,
                b'.' if !inside => {
                    steps.push(&text[start..at]);
                    start = at + 1;
                }
                b':' if !inside => {
                    end = at;
                    break;
                }
                _ => {}
            }
        }
        if inside {
            return Err(ParseError::Id(text.to_owned()));
        }
        let read = match &text[end..] {
            "" => false,
            READ => true,
            _ => return Err(ParseError::Level(text.to_owned())),
        };
        if text.is_empty() {
            return Err(ParseError::Empty);
        }
        steps.push(&text[start..end]);
        let path = steps
            .into_iter()
            .map(str::parse)
            .collect::<Result<Vec<Segment>, _>>()?;
        Self::new(path, read)
    }
}

#[cfg(test)]
mod tests {
    use super::{Scope, Segment};

    fn scope(text: &str) -> Scope {
        text.parse().unwrap()
    }

    #[test]
    fn text_round_trips() {
        for text in [
            "com.hashinbox",
            "com.hashinbox.wallets:read",
            "com.hashinbox.wallets[ethereum.mainnet.0xabc]:read",
            "com.hashinbox.wallets[a].indexes[b]",
            "com.hashinbox.wallets[a:read]",
        ] {
            assert_eq!(scope(text).to_string(), text);
        }
    }

    #[test]
    fn ids_are_escaped() {
        let odd = Segment::new("wallets", Some("a,b] c%d".to_owned())).unwrap();
        let odd = Scope::new(vec![odd], false).unwrap();
        let text = odd.to_string();
        assert_eq!(text, "wallets[a,b%5D%20c%25d]");
        assert_eq!(scope(&text), odd);
    }

    #[test]
    fn bad_text_is_refused() {
        for text in [
            "",
            ":read",
            "wallets:write",
            "wallets:read:read",
            "wallets.$read",
            "wallets.*",
            "wallets[]",
            "wallets[a",
            "wal lets",
            "wallets[%zz]",
        ] {
            assert!(text.parse::<Scope>().is_err(), "{text}");
        }
    }

    #[test]
    fn coverage() {
        let covers = |ours: &str, theirs: &str| scope(ours).covers(&scope(theirs));
        assert!(
            covers("a.wallets:read", "a.wallets[x]:read"),
            "all covers one"
        );
        assert!(covers("a.wallets[x]:read", "a.wallets[x]:read"));
        assert!(
            !covers("a.wallets[x]:read", "a.wallets:read"),
            "one, not all"
        );
        assert!(!covers("a.wallets[x]:read", "a.wallets[y]:read"));
        assert!(covers("a.wallets[x]", "a.wallets[x].tags"), "its subtree");
        assert!(covers("a.wallets:read", "a.wallets[x].tags:read"));
        assert!(
            !covers("a.wallets:read", "a.wallets[x].tags"),
            "the level carries"
        );
        assert!(
            !covers("a.wallets.tags", "a.wallets:read"),
            "not its parent"
        );
        assert!(covers("a", "a.wallets[x].tags"), "the root");
        assert!(covers("a.wallets", "a.wallets:read"), "full covers read");
        assert!(!covers("a.wallets:read", "a.wallets"));
    }
}
