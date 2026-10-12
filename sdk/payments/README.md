# payments

The container (053): every configured chain composed over one caller-supplied
store, its index driven, and an operational handle over its networks
(`Payments`, `Networks`). It names no chain: the application registers each
chain's `Config` by section name. As `payments::config`, it also reads the
configuration document and hands each named section to the chain that
registered for it, and holds the settings every chain shares: the RPC
endpoint list (`Rpc`) and the cadence (`Cadence`).

## The container

```rust
use std::sync::Arc;

use payments::{Payments, Split, config::Chain};
use serde::de::DeserializeOwned;

/// `C` is a chain crate's own `Config`, such as `chain_bitcoin::Config<String>`.
async fn run<C>(json: &[u8]) -> Result<(), Box<dyn std::error::Error>>
where
    C: Chain<String, Split> + DeserializeOwned,
{
    let repo = Arc::new(indexing::memory::Repository::new());
    let payments = Payments::<String>::from_json(json)?
        .with::<C>("bitcoin")
        .with_store(repo.clone()) // required: watches + index (+ purge)
        .with_output_store(repo.clone()) // optional slots; omit what is unused
        .start()
        .await?; // verifies nodes, composes, drives, waits for readiness
    let mut changes = payments.changes().expect("the default index slot notifies");
    let wallet = payments.wallets.create(&"btc".to_owned()).await?;
    payments.watch("alice", &wallet, indexing::Birthday::Fresh).await?;
    if let Ok(change) = changes.try_recv() {
        // The repository is the answer: re-read it.
        let _settled = repo.balance(wallet.as_ref())?;
        println!("{} advanced to {}", change.watch, change.block.height.0);
    }
    for network in payments.networks.list() {
        println!("{}: served {}", network.network.name, network.served);
    }
    payments.shutdown().await?;
    Ok(())
}
```

- Store slots: `with_store` (the required `WatchStore + IndexStore`),
  `with_output_store`, `with_wallet_registry`; each
  is a view of the caller's instance. An optional slot left empty answers its
  one consumer with an error naming the missing call. `with_index_store`
  replaces the container's notifying index view, and `changes()` then answers
  `None`. With `with_wallet_registry`, `start()` restores every served
  network's signers into `payments.wallets`; without it none are loaded.
- `Payments::builder().chain(config)` takes a chain's `Config` built in code;
  `from_json`/`from_toml` (feature `toml`) plus `.with::<C>(name)` parse it.
- `payments.networks`: `list`, `replace(network, endpoints, persist)` (verify,
  the caller's persistence, then swap the transport), `pause`, `resume`,
  `rescan` (the online rescan) and `health`.
- Balance and history are the repository's own reads, not the container's.
- The manual path (`config::Chains::compose`) stays for embedders who wire
  everything themselves.

## Configuration

```rust
use payments::config::{Chain, Chains, Document, Error, Registry, Sections};
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
    "rpc": { "endpoints": ["http://user:pass@127.0.0.1:18443"] },
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
holds: one `indexing::Indexers`, each network's pending blocks
(`indexing::Pending`) for an application completing its own stored reads, one
`wallets::Wallets` map and every declared unit; each index carries the poll
`Schedule` its chain declared. `Connection` is a verified chain with its type erased.
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
- An RPC endpoint is its URL, like a database URI: userinfo and query keys
  carry credentials; there are no headers, secret maps or `${NAME}` references.
- An application that keeps a network's nodes elsewhere (stored) reads each
  parsed section's endpoint documents, as written, with `Chains::endpoints` and
  hands its own back with `Chains::with_endpoints`; the section's `rpc.repeat`
  and chain keys stay as written. A chain parsed from a document is composed over
  a `json_rpc::Swap` (`ChainNodes::transport`). `ChainNodes::verify` runs the
  chain's own `verify` over candidate endpoints and returns the transport and
  counters to install with `Swap::replace`; nothing is parsed again.
- `Chains::with_transport_for` composes one network over a caller-owned transport
  instead; the caller then owns its endpoint counters.
