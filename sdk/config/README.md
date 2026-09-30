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

`Chains::compose` then verifies every node, opens storage and returns a
`wallets::Composition`. No document is required: `Chains::add` accepts a chain
`Config` built in code.

## Notes

- This crate depends on no chain crate. A section is accepted only if the
  application registered a chain under that exact name, so a document naming a
  chain the binary did not register fails at startup.
- TOML needs the `toml` feature, which also adds `Document::read`. Without it
  every document is parsed as JSON.
- A `${NAME}` reference is resolved when the transport is built, not when the
  document is parsed. A missing environment variable therefore fails at
  `Rpc::transport` or `Chains::connect`, after validation has passed.
