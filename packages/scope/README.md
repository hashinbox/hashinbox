# scope

Access scopes as text, what an API key or a token holds:

```text
com.hashinbox.wallets[ethereum.mainnet.0xabc].$read   read one wallet
com.hashinbox.wallets.$*                              any action on every wallet
com.hashinbox.wallets.*.$*                            any action on wallets and all below
com.hashinbox.*.$read                                 read everything
```

## Grammar

```text
scope   = step *("." step) ".$" action
step    = name [ "[" id "]" ] | "*"
action  = name | "*"
name    = 1*( ALPHA / DIGIT / "_" / "-" )
id      = any text; "%", "[", "]", whitespace and controls written as %XX
```

A step names one id at most. Several wallets are several scopes:
`wallets[a].$read wallets[b].$read`.

A set of scopes (`Scopes`) is written space-separated, as the JWT `scope` claim is.

## Coverage

`a.covers(b)` holds when `a` allows everything `b` does:

- the action is the same, or `a`'s is `$*`;
- the path matches step by step:
  - a name covers the same name;
  - `wallets` covers `wallets[x]`;
  - `wallets[x]` covers only `wallets[x]`: neither `wallets` nor `wallets[y]`;
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

Compression drops each scope another covers, then sorts, so a JWT carries no excess:

```rust
use scope::Scopes;

let held: Scopes = "com.hashinbox.wallets[w1].$read com.hashinbox.wallets[w2].$read \
                    com.hashinbox.markets[btc].$read com.hashinbox.markets.$read"
    .parse()
    .unwrap();
assert_eq!(
    held.compress().to_string(),
    "com.hashinbox.markets.$read com.hashinbox.wallets[w1].$read com.hashinbox.wallets[w2].$read"
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
assert_eq!(
    packed["com.hashinbox"],
    ["markets.$read", "wallets[w1].$read", "wallets[w2].$read"]
);
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

`scope!` writes a scope as code: an id in `[ ]` is an expression, and the action comes after `=>`.

`require!(granted, "template", name = expr, …)` returns `Err(Denied.into())` from the enclosing
function unless the granted scopes cover the template's scope:

- `{name}` takes the variable `name`, or the named argument `name = …`, as `format!` does;
- each value (`Display`) is escaped as an id, so `]`, `%` or whitespace in it stays inside the
  id;
- the filled text is parsed; text that does not parse is denied (a bug, never a panic);
- it works anywhere, any number of times: in loops, in branches; two wallets are two `require!`s.

```rust
use scope::{Denied, Scopes, require, scope};

fn balance(granted: &Scopes, wallet: &str) -> Result<&'static str, Denied> {
    require!(granted, "com.hashinbox.wallets[{wallet}].$read");
    Ok("1.5 BTC")
}

fn transfer(granted: &Scopes, from: &str, to: &str) -> Result<(), Denied> {
    require!(granted, "com.hashinbox.wallets[{from}].$write");
    require!(granted, "com.hashinbox.wallets[{id}].$write", id = to);
    Ok(())
}

let token: Scopes = "com.hashinbox.wallets[w1].$read".parse().unwrap();
assert_eq!(balance(&token, "w1"), Ok("1.5 BTC"));
assert_eq!(
    balance(&token, "w2").unwrap_err().to_string(),
    "missing scope com.hashinbox.wallets[w2].$read"
);
assert!(balance(&token, "w1].x[w2").is_err()); // escaped: one odd id, not two steps
let both: Scopes = "com.hashinbox.wallets[w1].$write com.hashinbox.wallets[w2].$write"
    .parse()
    .unwrap();
assert!(transfer(&both, "w1", "w2").is_ok() && transfer(&both, "w1", "w3").is_err());
assert_eq!(scope!(com.hashinbox.* => read).to_string(), "com.hashinbox.*.$read");
```

`require!` takes anything implementing `Granted` (a session, a key), or a reference to one.
`Scopes` implements it.

## Lists

A list route requires the unnarrowed scope (`com.hashinbox.wallets.$read`). A caller granted
only some ids reads each by its id.

## Endpoints: `#[scope::utoipa]` reads the `require!`s

`#[scope::utoipa(<utoipa::path args>)]` finds every `require!` in the handler's body and writes
their templates, in order, deduplicated and as written, into
`#[utoipa::path(…, security(("bearer" = [<templates>])))]`.

```rust
use futures_executor::block_on;
use scope::{Denied, Scopes, require};

#[scope::utoipa(get, path = "/v1/wallets/{id}")]
async fn read(granted: Scopes, id: String) -> Result<String, Denied> {
    require!(granted, "com.hashinbox.wallets[{id}].$read");
    Ok(id)
}

#[scope::utoipa(get, path = "/v1/wallets")]
async fn list(granted: Scopes) -> Result<(), Denied> {
    require!(granted, "com.hashinbox.wallets.$read");
    Ok(())
}

let token: Scopes = "com.hashinbox.wallets[w1].$read".parse().unwrap();
assert!(block_on(read(token.clone(), "w1".into())).is_ok());
assert!(block_on(read(token.clone(), "w2".into())).is_err());
assert!(block_on(list(token)).is_err());
```

- OpenAPI shows `com.hashinbox.wallets[{id}].$read` and `com.hashinbox.wallets.$read`.
- The crate using it depends on `utoipa` (the attribute emits `::utoipa::path`) and on `scope`.
- It does not compile when the body has no `require!`, `security(…)` is given, or a
  `require!`'s template is not a string literal.
- Only `require!`s written in the handler's body count: one in a function it calls still checks,
  but OpenAPI does not show it.

See `examples/token.rs` for a key, a token it signs and requests checked against it.
