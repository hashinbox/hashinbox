//! Hand-written: scopes are `packages/scope`'s own types, plus filling a
//! template's holes at run time.

pub use ::scope::{ParseError, Scope, Scopes};

/// `id` as written between a scope's brackets: `%`, `[`, `]`,
/// whitespace and controls as `%XX` (`packages/scope`'s escape).
pub fn escape_id(id: &str) -> String {
    ::scope::__escape_id(id)
}

/// `template` with each `{name}` what `value` gives for it:
/// `fill("a.wallets[{id}]:read", |_| escape_id("x"))`.
pub fn fill(template: &str, value: impl Fn(&str) -> String) -> String {
    let mut text = String::new();
    let mut rest = template;
    while let (Some(open), Some(close)) = (rest.find('{'), rest.find('}')) {
        text.push_str(&rest[..open]);
        text.push_str(&value(&rest[open + 1..close]));
        rest = &rest[close + 1..];
    }
    text + rest
}
