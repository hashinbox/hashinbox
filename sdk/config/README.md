# sdk-config

Reads a configuration document and hands each named section to the chain that
registered for it. It also holds the settings every chain shares: the RPC
endpoint list (`Rpc`) and the polling cadence (`Cadence`).

## Usage

```rust
use sdk_config::{Chain, Chains, Document, Error, Registry, Sections};
use serde::de::DeserializeOwned;

/// `C` is a chain crate's own `Config`, such as `chain_bitcoin::Config<AssetTag>`.
fn configured<C, A, R>(json: &[u8]) -> Result<Chains<A, R>, Error>
where
    C: Chain<A, R> + DeserializeOwned,
    A: 'static,
    R: 'static,
{
    let mut registry = Registry::new();
    registry.register::<C>("bitcoin");

    let sections: Sections = Document::json(json)?.parse()?;
    registry.chains(&sections)
}
```

The document for that registry has one section per registered name:

```json
{
  "bitcoin": {
    "network": "regtest",
    "confirmation_depth": 6,
    "rpc": { "endpoints": ["http://${BTC_RPC_USER}:${BTC_RPC_PASSWORD}@127.0.0.1:18443"] },
    "cadence": { "poll_millis": 1000, "batch_size": 250 }
  }
}
```

A chain's `Config` implements `Chain`: `network` names its network from the
section alone, `accept` validates local settings, `verify` checks a node's identity
over a transport, and `connect` builds the transport, verifies it and returns what
composes the chain.

`Chains::compose` then verifies every node, opens storage and returns a
`Composition`; the first failed verification ends it.

`Composition` folds every verified chain's `ComposedChain` into what a process
holds: one `indexing::Index`, one history and one balance handle routed by
network, one `wallets::Wallets` map, every declared unit and each network's
`Schedule`. `Connection` is a verified chain with its type erased.
`Composition::verify_then_build` finishes every identity check before storage
opens, so a deployment naming the wrong node writes nothing.
`Chains::compose_available` instead skips a chain whose node fails verification and
returns a `Startup`: the composition of the rest, each verified network's endpoint
counters, every configured network as a `ChainNodes`, and the skipped sections'
errors. No document is required: `Chains::add` accepts a chain `Config` built in
code.

## Notes

- This crate depends on no chain crate. A section is accepted only if the
  application registered a chain under that exact name, so a document naming a
  chain the binary did not register fails at startup.
- TOML needs the `toml` feature, which also adds `Document::read`. Without it
  every document is parsed as JSON.
- A `${NAME}` reference is resolved when the transport is built, not when the
  document is parsed. A missing environment variable therefore fails at
  `Rpc::transport` or `Chains::connect`, after validation has passed.
- An application that keeps a network's nodes elsewhere (stored) reads each
  parsed section's endpoint documents, as written, with `Chains::endpoints` and
  hands its own back with `Chains::with_endpoints`; the section's `rpc.repeat`
  and chain keys stay as written. A chain parsed from a document is composed over
  a `json_rpc::Swap` (`ChainNodes::transport`). `ChainNodes::verify` runs the
  chain's own `verify` over candidate endpoints and returns the transport and
  counters to install with `Swap::replace`; nothing is parsed again.
- `Chains::with_transport_for` composes one network over a caller-owned transport
  instead; the caller then owns its endpoint counters.
