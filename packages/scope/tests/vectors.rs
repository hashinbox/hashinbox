//! The conformance vectors the client ports of this crate read
//! (`apps/server/clients/_golden/scope.json`), written from this crate's own
//! answers. Fails when the file is stale; `BLESS=1` rewrites it.

use scope::{Scope, Scopes};
use serde_json::{Value, json};

/// Texts that parse, with odd ids and every kind of step.
const PARSED: &[&str] = &[
    "com.hashinbox",
    "com.hashinbox:read",
    "com.hashinbox.wallets:read",
    "com.hashinbox.wallets[ethereum.mainnet.0xabc]:read",
    "com.hashinbox.wallets[a].indexes[b].legs:read",
    "com.hashinbox.wallets[a,b%5D%20c%25d]",
    "wallets[%61%62]:read",
    "wallets[%+5]:read",
    "wallets[a]b[c]:read",
    "wallets[a[b]:read",
    "wallets[a:read]",
    "wallets[a:b]:read",
    "wallets[%C3%A9]:read",
    "wallets[é ü]:read",
    "a-b_c",
];

/// Texts that do not parse.
const REFUSED: &[&str] = &[
    "",
    ":read",
    "wallets:",
    "wallets:write",
    "wallets:read:read",
    "wallets:read.tags",
    "wallets.$read",
    "wallets.$*",
    "wallets.*",
    "*",
    "wallets[]",
    "wallets[a",
    "wal lets",
    "wallets[%zz]",
    "wallets[%4]",
    "wallets[%-5]",
    "wallets[%C3]",
    "wallets[a]x",
    ".wallets",
    "wallets.",
    "wallets..tags",
];

/// `(ours, theirs)` pairs for `covers`.
const COVERS: &[(&str, &str)] = &[
    ("a.wallets:read", "a.wallets[x]:read"),
    ("a.wallets[x]:read", "a.wallets[x]:read"),
    ("a.wallets[x]:read", "a.wallets:read"),
    ("a.wallets[x]:read", "a.wallets[y]:read"),
    ("a.wallets[x]", "a.wallets[x].tags"),
    ("a.wallets[x]", "a.wallets[x].indexes[b].legs:read"),
    ("a.wallets:read", "a.wallets[x].tags:read"),
    ("a.wallets:read", "a.wallets[x].tags"),
    ("a.wallets.tags", "a.wallets"),
    ("a.wallets.tags:read", "a.wallets:read"),
    ("a.wallets", "a.wallets:read"),
    ("a.wallets:read", "a.wallets"),
    ("a", "a.b[c].d"),
    ("a:read", "a.b[c].d:read"),
    ("a:read", "a.b"),
    ("a.b", "a"),
    ("a.b", "b"),
];

/// Sets of scopes (space-separated) to compress and pack.
const SETS: &[&str] = &[
    "",
    "a.wallets[x]:read a.wallets[y]:read a.wallets[x]:read a.markets:read \
     a.markets[btc]:read a.wallets[z] a.wallets[z].tags",
    "com.hashinbox.wallets[x]:read com.hashinbox.wallets[y]:read \
     com.hashinbox.wallets[x].tags com.hashinbox.triggers other.thing",
    "a.b.c:read a.b:read",
    "a.b.c a.b:read",
    "com.hashinbox com.hashinbox.wallets:read",
    "com.hashinbox:read com.hashinbox.wallets",
    "a.b[x]:read a.b[y]:read a.b.c:read",
    "x[é]:read x[z]:read x[ä]:read x[%F0%9F%98%80]:read x[%EF%BC%A1]:read",
    "123.a:read 123.b:read 9.a:read",
    "a:read a a.b",
];

/// Packed claims to unpack, in either form: a group's text or list, `*`
/// its prefix itself.
const UNPACKS: &[&str] = &[
    r#"{"com":["hashinbox"]}"#,
    r#"{"com.hashinbox":"*"}"#,
    r#"{"com.hashinbox":["*:read","wallets"],"":"x[y]"}"#,
    r#"{"a":"b.c:read","a.b":["*"]}"#,
    r#"{"":"*"}"#,
    r#"{"a":"*b"}"#,
    r#"{"a":"*:write"}"#,
];

/// `(granted, asked)` pairs for `first_uncovered`.
const NARROWS: &[(&str, &str)] = &[
    ("a.wallets a.markets:read", "a.wallets[x]:read"),
    (
        "a.wallets a.markets:read",
        "a.wallets[x]:read a.networks:read",
    ),
    ("", "a:read"),
    ("a:read", ""),
];

/// What holding a scope implies beyond its subtree, for `expand`.
const DEPENDS: &[(&str, &[&str])] = &[
    ("a.triggers", &["a.wallets"]),
    ("a.wallets", &["a.jobs"]),
    ("a.networks.endpoints", &["a.networks:read"]),
    (
        "a.networks.routes",
        &["a.networks.endpoints:read", "a.logs"],
    ),
];

/// Sets of scopes to `expand` with [`DEPENDS`].
const EXPANDS: &[&str] = &[
    "",
    "a.triggers",
    "a.triggers:read",
    "a.triggers[t1]",
    "a.triggers[t1].runs:read",
    "a.wallets[w1]:read",
    "a.networks[x].endpoints[y]",
    "a.networks.routes",
    "a.networks.routes:read",
    "a.networks",
    "a",
    "a:read",
    "a.wallets:read a.jobs",
    "a.triggers:read a.wallets",
    "b.triggers",
];

/// Ids to escape.
const IDS: &[&str] = &[
    "a,b] c%d",
    "ethereum.mainnet.0xabc",
    "hip/abc",
    "tab\there\nnew\x7f\x00",
    "[é]",
    "\u{85}\u{a0}",
];

fn scope(text: &str) -> Scope {
    text.parse().unwrap()
}

fn scopes(text: &str) -> Scopes {
    text.parse().unwrap()
}

fn vectors() -> Value {
    let parse: Vec<Value> = PARSED
        .iter()
        .map(|text| json!({ "text": text, "display": scope(text).to_string() }))
        .collect();
    for text in REFUSED {
        assert!(text.parse::<Scope>().is_err(), "{text}");
    }
    let covers: Vec<Value> = COVERS
        .iter()
        .map(|(ours, theirs)| {
            json!({ "ours": ours, "theirs": theirs, "covers": scope(ours).covers(&scope(theirs)) })
        })
        .collect();
    let sets: Vec<Value> = SETS
        .iter()
        .map(|text| {
            let held = scopes(text);
            let packed = held.pack();
            assert_eq!(Scopes::unpack(&packed).unwrap(), held.compress(), "{text}");
            json!({
                "scopes": text,
                "display": held.to_string(),
                "compressed": held.compress().to_string(),
                "packed": packed,
            })
        })
        .collect();
    let unpacks: Vec<Value> = UNPACKS
        .iter()
        .map(|text| {
            let packed: scope::Packed = serde_json::from_str(text).unwrap();
            let scopes = Scopes::unpack(&packed).ok().map(|held| held.to_string());
            json!({ "packed": packed, "scopes": scopes })
        })
        .collect();
    let narrows: Vec<Value> = NARROWS
        .iter()
        .map(|(granted, asked)| {
            let asked = scopes(asked);
            let first = scopes(granted)
                .first_uncovered(&asked)
                .map(ToString::to_string);
            json!({ "granted": granted, "asked": asked.to_string(), "first_uncovered": first })
        })
        .collect();
    let depends: serde_json::Map<String, Value> = DEPENDS
        .iter()
        .map(|(source, targets)| ((*source).to_owned(), json!(targets)))
        .collect();
    let expands: Vec<Value> = EXPANDS
        .iter()
        .map(|text| json!({ "scopes": text, "expanded": scopes(text).expand(DEPENDS).to_string() }))
        .collect();
    let escape: Vec<Value> = IDS
        .iter()
        .map(|id| json!({ "id": id, "text": scope::__escape_id(id) }))
        .collect();
    json!({
        "parse": parse,
        "refuse": REFUSED,
        "covers": covers,
        "sets": sets,
        "unpack": unpacks,
        "first_uncovered": narrows,
        "expand": { "depends": depends, "cases": expands },
        "escape": escape,
    })
}

#[test]
fn vectors_are_current() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../apps/server/clients/_golden/scope.json"
    );
    let text = serde_json::to_string_pretty(&vectors()).unwrap() + "\n";
    if std::env::var_os("BLESS").is_some() {
        std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
        std::fs::write(path, &text).unwrap();
        return;
    }
    let kept = std::fs::read_to_string(path).unwrap_or_default();
    assert!(
        kept == text,
        "{path} is stale: run `BLESS=1 cargo test -p scope --test vectors`"
    );
}
