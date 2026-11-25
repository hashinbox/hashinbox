# scope

Access scopes as text, what an API key or a token holds:

```text
com.hashinbox.wallets[ethereum.mainnet.0xabc].$read   read one wallet
com.hashinbox.wallets[a,b].$read                      read two wallets
com.hashinbox.wallets.$*                              any action on every wallet
com.hashinbox.wallets.*.$*                            any action on wallets and all below
com.hashinbox.*.$read                                 read everything
```

## Grammar

```text
scope   = step *("." step) ".$" action
step    = name [ "[" id *("," id) "]" ] | "*"
action  = name | "*"
name    = 1*( ALPHA / DIGIT / "_" / "-" )
id      = any text; "%", "[", "]", ",", whitespace and controls written as %XX
```

A set of scopes (`Scopes`) is written space-separated, as the JWT `scope` claim is.

## Coverage

`a.covers(b)` holds when `a` allows everything `b` does:

- the action is the same, or `a`'s is `$*`;
- the path matches step by step:
  - a name covers the same name;
  - `wallets` covers `wallets[x]`;
  - `wallets[x,y]` covers `wallets[x]`;
  - `wallets[x]` covers neither `wallets` nor `wallets[y]`;
- `*` in the middle covers any one step;
- `*` at the end covers anything below, the step before it included: `wallets.*` covers `wallets`,
  `wallets[x]` and `wallets[x].legs`;
- nothing else covers a longer path: `wallets[x]` does not cover `wallets[x].legs`.

A token may only narrow its key. `key.first_uncovered(&token)` is `None` when every token scope is
covered:

```rust
use scope::Scopes;

let key: Scopes = "com.hashinbox.wallets.$* com.hashinbox.markets.$read".parse().unwrap();
let token: Scopes = "com.hashinbox.wallets[w1].$read".parse().unwrap();
assert_eq!(key.first_uncovered(&token), None);

let wider: Scopes = "com.hashinbox.networks.$read".parse().unwrap();
assert!(key.first_uncovered(&wider).is_some());
```

## Compression

Compression gives the fewest scopes allowing the same thing, so a JWT carries no excess:

- a scope covered by another is dropped;
- scopes differing only in one step's ids are merged.

```rust
use scope::Scopes;

let held: Scopes = "com.hashinbox.wallets[w1].$read com.hashinbox.wallets[w2].$read \
                    com.hashinbox.markets[btc].$read com.hashinbox.markets.$read"
    .parse()
    .unwrap();
assert_eq!(
    held.compress().to_string(),
    "com.hashinbox.markets.$read com.hashinbox.wallets[w1,w2].$read"
);
```

`pack` compresses, then groups the scopes by their shared prefix, for the shortest JWT claim.
`Scopes::unpack` reverses it:

```rust
use scope::Scopes;

let held: Scopes = "com.hashinbox.wallets[w1].$read com.hashinbox.wallets[w2].$read \
                    com.hashinbox.markets.$read"
    .parse()
    .unwrap();
let packed = held.pack();
assert_eq!(packed["com.hashinbox"], ["markets.$read", "wallets[w1,w2].$read"]);
assert_eq!(Scopes::unpack(&packed).unwrap(), held.compress());
```

## Schema

`schema!` declares what exists. The schema expands wildcards to concrete scopes and refuses
typos:

```rust
scope::schema! {
    /// Every hashinbox scope.
    pub static HASHINBOX = com.hashinbox {
        wallets => [read, write],
        wallets.legs => [read],
        markets => [read],
    }
}

let read: scope::Scope = "com.hashinbox.*.$read".parse().unwrap();
assert_eq!(
    HASHINBOX.expand(&read).to_string(),
    "com.hashinbox.wallets.$read com.hashinbox.wallets.legs.$read com.hashinbox.markets.$read"
);

let typo: scope::Scope = "com.hashinbox.wallet.$read".parse().unwrap();
assert!(!HASHINBOX.knows(&typo));
```

## Macros

`scope!` writes a scope as code: ids in `[ ]` are expressions, and the action comes after `=>`.
`require!` returns `Err(Denied.into())` from the enclosing function unless the granted scopes
cover it:

```rust
use scope::{Denied, Scopes, require, scope};

fn balance(granted: &Scopes, wallet: &str) -> Result<&'static str, Denied> {
    require!(granted, com.hashinbox.wallets[wallet] => read);
    Ok("1.5 BTC")
}

let token: Scopes = "com.hashinbox.wallets[w1].$read".parse().unwrap();
assert_eq!(balance(&token, "w1"), Ok("1.5 BTC"));
assert_eq!(
    balance(&token, "w2").unwrap_err().to_string(),
    "missing scope com.hashinbox.wallets[w2].$read"
);
assert_eq!(scope!(com.hashinbox.* => read).to_string(), "com.hashinbox.*.$read");
```

See `examples/token.rs` for a key, a token it signs and requests checked against it.
