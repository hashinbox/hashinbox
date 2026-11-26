//! The conformance vectors the client ports of this crate read
//! (`apps/server/clients/_golden/scope.json`), written from this crate's own
//! answers. Fails when the file is stale; `BLESS=1` rewrites it.

use scope::{Scope, Scopes};
use serde_json::{Value, json};

/// Texts that parse, with odd ids and every kind of step.
const PARSED: &[&str] = &[
    "com.hashinbox.wallets.$read",
    "com.hashinbox.wallets[ethereum.mainnet.0xabc].$read",
    "com.hashinbox.wallets[a].legs[b].legs.$read",
    "com.hashinbox.*.$read",
    "com.hashinbox.wallets.*.$*",
    "com.hashinbox.wallets[a,b%5D%20c%25d].$*",
    "wallets[%61%62].$read",
    "wallets[%+5].$read",
    "wallets[a]b[c].$read",
    "wallets[a[b].$read",
    "wallets[%C3%A9].$read",
    "wallets[é ü].$read",
    "a-b_c.$x-y_z",
];

/// Texts that do not parse.
const REFUSED: &[&str] = &[
    "",
    "$read",
    "wallets",
    "wallets.read",
    "wallets[].$read",
    "wallets[a.$read",
    "wal lets.$read",
    "wallets.$re ad",
    "wallets[%zz].$read",
    "wallets[%4].$read",
    "wallets[%-5].$read",
    "wallets[%C3].$read",
    "wallets[a]x.$read",
    ".wallets.$read",
    "wallets..$read",
    "wallets.$",
    "wallets.$**",
    "*[a].$read",
];

/// `(ours, theirs)` pairs for `covers`.
const COVERS: &[(&str, &str)] = &[
    ("a.wallets.$read", "a.wallets[x].$read"),
    ("a.wallets[x].$read", "a.wallets[x].$read"),
    ("a.wallets[x].$read", "a.wallets.$read"),
    ("a.wallets[x].$read", "a.wallets[y].$read"),
    ("a.wallets[x].$read", "a.wallets[x].legs.$read"),
    ("a.*.$read", "a.wallets[x].legs.$read"),
    ("a.wallets.*.$read", "a.wallets.$read"),
    ("a.*.legs.$read", "a.wallets.legs.$read"),
    ("a.*.legs.$read", "a.wallets.x.legs.$read"),
    ("a.wallets.$*", "a.wallets.$write"),
    ("a.wallets.$read", "a.wallets.$*"),
    ("a.wallets.$read", "a.wallets.$write"),
    ("a.wallets.legs.$read", "a.wallets.*.$read"),
    ("*.$*", "a.b[c].d.$read"),
    ("a.*.$read", "a.$read"),
    ("a.$read", "a.b.$read"),
];

/// Sets of scopes (space-separated) to compress and pack.
const SETS: &[&str] = &[
    "",
    "a.wallets[x].$read a.wallets[y].$read a.wallets[x].$read a.markets.$read \
     a.markets[btc].$read a.wallets[z].$write",
    "com.hashinbox.wallets[x].$read com.hashinbox.wallets[y].$read \
     com.hashinbox.wallets[x].legs.$read com.hashinbox.markets.$read other.thing.$write",
    "a.b.c.$read a.b.$read",
    "a.wallets[x].$read a.wallets[y].$read a.*.$write a.wallets.$write",
    "com.hashinbox.wallets.*.$* com.hashinbox.markets.$read",
    "*.$read a[x].b.$read b.$* *.c.$write",
    "a.b[x].$read a.b[y].$read a.b.c.$read",
    "x[é].$read x[z].$read x[ä].$read x[%F0%9F%98%80].$read x[%EF%BC%A1].$read",
    "123.a.$read 123.b.$read 9.a.$read",
    "a.$read a.$* a.$write",
];

/// `(granted, asked)` pairs for `first_uncovered`.
const NARROWS: &[(&str, &str)] = &[
    ("a.wallets.$* a.markets.$read", "a.wallets[x].$read"),
    (
        "a.wallets.$* a.markets.$read",
        "a.wallets[x].$read a.networks.$read",
    ),
    ("", "a.$read"),
    ("a.$read", ""),
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
    let escape: Vec<Value> = IDS
        .iter()
        .map(|id| json!({ "id": id, "text": scope::__escape_id(id) }))
        .collect();
    json!({
        "parse": parse,
        "refuse": REFUSED,
        "covers": covers,
        "sets": sets,
        "first_uncovered": narrows,
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
