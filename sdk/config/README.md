# sdk-config: one document, each chain reading its own section

## Why this crate exists

**A chain is configured in Rust; a document is sugar on top.** An embedder
writes `chain_bitcoin::Config { .. }`, calls `validate` and `connect`, and
links no parser at all. This crate adds the other way in — a file, or JSON
bytes, or a `&str` — and both ways reach the same value, checked by the same
`validate` ([`docs/design/030 §8 (ex-028)`](../../docs/design/030 §8 (ex-028).md)).

It owns the **document**: reading it, splitting it into named sections, and
saying *which section* failed and why. It owns no chain's meaning. Each chain
owns its own section by implementing [`Chain`] for its own `Config`, and
registration is the chain handing itself in — which is why **nothing here
depends on a chain crate**, and why adding a fourth chain is one `register`
line in a composition root rather than an edit to this crate, to `apps/server`,
or to any scenario.

It used to live in `packages/config`, where the layer rule "a package may
depend only on packages" meant it could not name `base::Network` or
`indexing::SyncPolicy`. The types were contorted to fit: `Sync::poll_millis`
was a `u64` each chain multiplied back into a `Duration`, and
`Sync::batch_size` a `usize` each chain rebuilt into a `SyncPolicy`. Under
`sdk/` they are the types their consumers take, and the re-inflation at three
chain ends is gone.

No type here derives `Serialize`, and [`Rpc`]'s, [`Sections`]'s, [`Chains`]'s
and `Document`'s `Debug` are written rather than derived. A URL may carry a
credential inline, and a configuration value that can print itself is a
configuration value that ends up in a log — the same rule `json_rpc::Endpoint`
already follows.

## Concepts

| Concept | Type | Notes |
|---|---|---|
| which chain owns which section | [`Registry`] | `register::<C>(name)`; the whole coupling between a document and its chains |
| a chain's own section, erased | [`Chain`] | three methods a chain crate implements for its `Config`; no associated types |
| every configured chain | [`Chains`] | from a document, or from [`Chains::add`] with no document at all |
| the sections, still raw | [`Sections`] | `name -> value`; this crate never looks inside one |
| the nodes, and how hard to call them | [`Rpc`] | one `json_rpc::Endpoint` per node; failover is the list's order |
| how much one call repeats | [`Repetition`] | `"off"`, or an explicit [`RepetitionPolicy`]; a property of the list, not of a node |
| how fast to walk blocks | [`Cadence`] | a `Duration` and an `indexing::SyncPolicy`; no confirmation line — the chain states that |
| the composed transport | [`Transport`] | `Arc<dyn json_rpc::Client>`, the same handle on every chain |
| a file, when a file is wanted | `Document` | JSON always; TOML behind feature `toml`, off by default |
| a rejected value | [`Error`], [`ErrorKind`] | carries the section at fault; never quotes a URL, a header or a resolved secret |

Validation is a method on the value, never a `deserialize_with`. A check that
runs while parsing is unreachable to a caller who builds the value in code, so
every check that is about a *value* lives in `validate`.

## Public API map

| Group | Items |
|---|---|
| register | [`Registry::new`], [`Registry::register`], [`Registry::chains`] |
| build in code | [`Chains::new`], [`Chains::add`], [`Rpc::new`], [`Rpc::endpoint`], [`Cadence::new`], [`RepetitionPolicy::new`] |
| check | [`Rpc::validate`], [`Repetition::validate`], [`Cadence::validate`] |
| compose | [`Chains::networks`], [`Chains::notices`], [`Chains::connect`], [`Chains::compose`], [`Rpc::transport`] |
| read a document | `Document::json`; `Document::toml` and `Document::read` behind feature `toml` |
| errors | [`Error`], [`ErrorKind`] |

## Usage

A composition root names each chain once and nothing else in the process names
one:

```rust,ignore
let mut registry = sdk_config::Registry::new();
registry.register::<chain_bitcoin::Config<AssetTag>>("bitcoin");
registry.register::<chain_ethereum::Config<AssetTag>>("ethereum");
registry.register::<chain_solana::Config<AssetTag>>("solana");

let document = sdk_config::Document::read("payments.toml").await?;
let settings: MyConfiguration = document.parse()?;
let chains = registry.chains(&settings.chains)?;   // each parsed by its own chain
for notice in chains.notices() {
    println!("{notice}");
}
let composition = chains
    .compose(&secrets, || storage.open(), MyStore::repository, &registrar)
    .await?;
```

The document that feeds it. Every chain's section has the same shape: its own
keys, one `rpc`, one optional `cadence`.

```toml
# payments.toml
[chains.bitcoin]
network = 'regtest'
confirmation_depth = 6
rpc = { endpoints = ['http://${BTC_RPC_USER}:${BTC_RPC_PASSWORD}@127.0.0.1:18443'] }
cadence = { poll_millis = 1000, batch_size = 250 }

[chains.ethereum]
network = 'mainnet'
chain_id = 1
# A node that needs a header or its own bounds writes the object form.
rpc = { endpoints = [{ url = 'https://node.invalid/v1',
                       headers = { 'X-API-Key' = '${RPC_KEY}' } }],
        repeat = { attempts = 3, backoff_ms = 250, max_backoff_ms = 2000 } }
```

**No file is required.** The same chains, stated in code, reach the same
composition and link no parser:

```rust,ignore
let mut chains = sdk_config::Chains::new();
chains.add("bitcoin", chain_bitcoin::Config::new(Network::Regtest, rpc, "btc"))?;
```

Public fields plus a `new` that fills the defaults: override one setting with
struct-update syntax and restate nothing else. The functions `serde` calls for
an absent key are the functions `new` calls, so a parsed value and a built one
cannot drift.

```rust
use std::time::Duration;

use json_rpc::Endpoint;
use sdk_config::{Cadence, Repetition, RepetitionPolicy, Rpc};

# fn main() -> Result<(), sdk_config::Error> {
// One node, the transport's own repetition.
let rpc = Rpc::endpoint("http://${BTC_RPC_USER}:${BTC_RPC_PASSWORD}@127.0.0.1:18443");
rpc.validate()?;

// Two nodes, each with its own credential: one the provider puts in the URL,
// one it wants in a header. No repetition at all.
let failover = Rpc {
    repeat: Repetition::Off,
    ..Rpc::new(vec![
        Endpoint::new("https://primary.invalid/v3/${PRIMARY_KEY}"),
        Endpoint::new("https://backup.invalid/v1").with_header("X-API-Key", "${BACKUP_KEY}"),
    ])
};
failover.validate()?;

// One bound of the default policy, changed.
let patient = Rpc {
    repeat: Repetition::Policy(RepetitionPolicy { attempts: 7.try_into().expect("seven is nonzero"),
                                                  ..RepetitionPolicy::new() }),
    ..Rpc::endpoint("http://127.0.0.1:8545")
};
assert_eq!(patient.endpoint_count(), 1);

// A faster pass, on the default cadence.
let cadence = Cadence { policy: indexing::SyncPolicy::new(250), ..Cadence::new() };
cadence.validate()?;
assert_eq!(cadence.poll, Duration::from_secs(1));
# Ok(())
# }
```

A `${NAME}` is resolved when the transport is composed, not when the value is
built, so a configuration file never holds a secret:

```rust
use std::collections::BTreeMap;
use sdk_config::Rpc;

# fn main() -> Result<(), sdk_config::Error> {
let rpc = Rpc::endpoint("https://node.invalid/v1?apikey=${RPC_KEY}");
let secrets = BTreeMap::from([("RPC_KEY".to_owned(), "s3cret".to_owned())]);
rpc.transport(&secrets)?;

// An unset variable fails, and the message names neither the variable nor
// the URL.
let error = rpc.transport(&BTreeMap::new()).err().expect("RPC_KEY is unset");
assert_eq!(error.to_string(), "a configured endpoint's environment variable is unavailable");
# Ok(())
# }
```

## Related crates

| Layer (`linter.toml`) | Crates | Relation |
|---|---|---|
| packages | `json-rpc` | [`Rpc`] holds its `Endpoint` values, and [`Rpc::transport`] composes its `Http`, `Failover` and `Retry` into one [`Transport`] |
| base, indexing | `base`, `indexing` | [`Chain::network`] answers in `base::Network`; [`Cadence::policy`] *is* `indexing::SyncPolicy` |
| wallets | `wallets` | [`Chain::connect`] hands back a `wallets::Connection`, and [`Chains::compose`] is `wallets::Composition::verify_then_build` with the first phase already wired |
| chains | `chain-bitcoin`, `chain-ethereum`, `chain-solana` | each chain's own `Config<A>` holds an [`Rpc`] and a [`Cadence`] and implements [`Chain`]; **this crate names none of them, and none of them enables the `toml` feature** |
| server, examples | `payment-server`, `payment-examples` | the binary registers three chains and reads a TOML document; the scenarios build [`Chains`] in code and read nothing |

Design notes:
[`docs/design/030 §8 (ex-028).md`](../../docs/design/030 §8 (ex-028).md),
[`a retired design note (023).md`](../../a retired design note (023).md),
[`docs/design/030-the-proposal.md`](../../docs/design/030-the-proposal.md) step 11,
[`ARCHITECTURE.md`](../../ARCHITECTURE.md).
