# payments

The container: every configured network built over the application's
stores. Per network it keeps the `base::Service` its chain's `base::Config`
built, and the `rpc::Swap` every part of that service reaches its nodes
through. It names no chain; every call is keyed by network.

- `Document`: `{ networks: [{ chain, ..chain's own fields }] }`, read with
  `from_json`, `from_toml` (feature `toml`) or `from_value`. Each chain's
  section carries its own `interval_ms` (the indexer's wake period).
- `Payments::builder()`: `with_chain::<C: base::Config>(name)` registers the
  type that reads sections whose `chain` is `name`; `with_config(document)`;
  `with_network(config)` adds one in code; `with_client(network, client)`
  reaches that network through `client` instead of the chain's own
  `Config::client()`; `with_storage(base::Storage)` (required). `build()`
  wraps each network's transport in a `Swap` and builds the service over it.
- `Payments`:
  - services: `service(network)`, `services()`, `networks()`, `assets()`
    (merged), `client(network)` (the `Swap`: `replace` changes nodes live,
    without verifying them);
  - indexing: `run()` → `base::Handles` (the caller stops and joins),
    `watch(address, asset, birthday)` on the address's network, `stats()`;
  - wallets: `generate(network)` (kept in the wallet store),
    `wallet(address)`, `wallets()` (restored through the factories),
    `parse(network, text)`, `broadcast(network, signed)`.

```rust
use std::sync::Arc;

use base::{Config, Storage};
use payments::{Document, Payments};
use serde::de::DeserializeOwned;

/// `C` reads one chain's section.
async fn serve<C: Config + DeserializeOwned + 'static>(
    json: &str,
    storage: Storage,
    other_nodes: Arc<dyn rpc::Client>,
) -> anyhow::Result<()> {
    let payments = Payments::builder()
        .with_chain::<C>("bitcoin")
        .with_config(Document::from_json(json)?)
        .with_storage(storage)
        .build()
        .await?;
    for (network, stats) in payments.stats() {
        println!("{}: {stats:?}", network.name);
    }
    let handles = payments.run();
    if let Some(network) = payments.networks().next() {
        let (wallet, _secret) = payments.generate(network).await?;
        println!("new wallet {}", wallet.address().value);
        // Every part of the network moves to the other nodes at once.
        payments.client(network)?.replace(other_nodes, Vec::new());
    }
    handles.stop();
    handles.join().await
}
```
